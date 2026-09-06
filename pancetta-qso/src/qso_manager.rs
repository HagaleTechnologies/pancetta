//! QSO state machine and management
//!
//! This module provides the core QSO management functionality including
//! state transitions, timeout handling, and QSO lifecycle management.

use crate::async_database::QsoDatabase;
use crate::states::*;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration as TokioDuration, Interval};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Usable FT8 audio passband for our TX offset (Hz). Matches the collision
/// detector's clamp range in `autonomous.rs`.
pub const TX_OFFSET_MIN_HZ: f64 = 300.0;
/// Upper bound of the usable FT8 audio passband for our TX offset (Hz).
pub const TX_OFFSET_MAX_HZ: f64 = 2700.0;

/// Widest audio offset an **already-active** QSO may legitimately sit on (Hz),
/// and therefore the defensive clamp band for
/// [`QsoManager::apply_tx_offset_switch`].
///
/// Deliberately WIDER than [`TX_OFFSET_MIN_HZ`]/[`TX_OFFSET_MAX_HZ`], which
/// govern where we autonomously *pick a fresh* offset — not where an
/// in-progress QSO is allowed to be. Answering a CQ ties our reply to wherever
/// we decoded the DX, unclamped, for any DX up to ~2900 Hz (see the identical
/// reasoning on `DRIFT_CANDIDATE_MIN_HZ`/`MAX_HZ` in the frequency-drift
/// candidate gate). Clamping an offset action to the narrower *pick* band
/// would therefore be actively wrong for `OffsetAction::Revert`, whose
/// `target_hz` is a `last_known_good_offset_hz` that may itself be such an
/// unclamped reply offset: reverting a QSO to 2700 when it demonstrably worked
/// at 2850 defeats the point of reverting at all.
///
/// The real constraint is the modulator's transmittable envelope
/// (`pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` = 3100.0, which covers
/// a 2900 Hz base plus the widest FT2 tone spread).
pub const ACTIVE_QSO_TX_OFFSET_MIN_HZ: f64 = 200.0;
/// Upper bound of [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]'s band. See its doc comment.
pub const ACTIVE_QSO_TX_OFFSET_MAX_HZ: f64 = 2900.0;

/// Below this much movement (Hz), an "offset switch" relocates nothing.
///
/// The pre-existing float-noise tolerance
/// [`QsoManager::apply_tx_offset_switch`]'s `partner_freq` latch already used
/// (borrowed in turn from `compute_manual_tx_offset`'s `tx_off != dx_freq`
/// test), promoted to a named constant so the no-op REFUSAL added for PAN-79 /
/// PAN-72 round 3 and that latch can never disagree about what counts as a
/// real move. Well below the allocator's `min_separation_hz` (75 Hz default),
/// so it can never reject a genuine relocation.
const TX_OFFSET_NOOP_TOLERANCE_HZ: f64 = 1.0;

/// Hound calling region (low): Hounds call the Fox in 300–900 Hz.
const HOUND_CALL_MIN_HZ: f64 = 300.0;
const HOUND_CALL_MAX_HZ: f64 = 900.0;
/// Hound response region (post-QSY): after the Fox answers, the Hound moves up
/// to 1000–2700 Hz to send its R-report.
const HOUND_RESPONSE_MIN_HZ: f64 = 1000.0;
const HOUND_RESPONSE_MAX_HZ: f64 = 2700.0;

/// Audio-offset boundaries for FT8 Hound (DXpedition chaser) mode.
///
/// Carried inside [`QsoManagerConfig`] so callers (e.g. the coordinator) can
/// wire in operator-configured ranges from `pancetta-config::HoundConfig`
/// without introducing a cross-crate dependency (pancetta-qso is a lower crate
/// and must not depend on pancetta-config).
///
/// The defaults match the module-level `HOUND_*` constants (300/900/1000/2700).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoundRegions {
    /// Low calling-region minimum audio offset (Hz). Default 300.
    pub call_min_hz: f64,
    /// Low calling-region maximum audio offset (Hz). Default 900.
    pub call_max_hz: f64,
    /// Post-QSY response-region minimum audio offset (Hz). Default 1000.
    pub response_min_hz: f64,
    /// Post-QSY response-region maximum audio offset (Hz). Default 2700.
    pub response_max_hz: f64,
}

impl Default for HoundRegions {
    fn default() -> Self {
        Self {
            call_min_hz: HOUND_CALL_MIN_HZ,
            call_max_hz: HOUND_CALL_MAX_HZ,
            response_min_hz: HOUND_RESPONSE_MIN_HZ,
            response_max_hz: HOUND_RESPONSE_MAX_HZ,
        }
    }
}

/// PAN-17: is `callsign` representable in *any* FT8 wire format the encoder
/// supports? Mirrors `pancetta_ft8::encoder`'s i3=4 nonstandard-callsign
/// "exact" field constraint (a 58-bit base-38 pack: at most 11 characters
/// from `" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/"`) without pulling in a
/// runtime dependency on pancetta-ft8 (which pancetta-qso only depends on
/// optionally, for the `sim-hifi` harness). `pack28`'s standard-callsign
/// charset is a strict subset of this one, so a callsign failing this check
/// cannot be encoded by EITHER path — it genuinely can never be
/// transmitted, which is what lets [`QsoManager::check_timeouts_at`] retire
/// a QSO stuck on one immediately instead of waiting on the generic
/// timeout/watchdog timers (see the PAN-17 check there for why).
fn callsign_is_wire_representable(callsign: &str) -> bool {
    const HASH_CALL_CHARSET: &str = " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
    let upper = callsign.to_ascii_uppercase();
    upper.len() <= 11 && upper.chars().all(|c| HASH_CALL_CHARSET.contains(c))
}

/// PAN-17 round 3 (Codex re-review of #248, finding 1): is `callsign`
/// *plausibly* representable via `pack28` (the i3=1 standard path — the
/// ONLY path that can carry a numeric report/ack; the i3=4 nonstandard-
/// callsign path's 2-bit report field only has room for
/// blank/RRR/RR73/73, never a real dB value — see
/// `pancetta_ft8::encoder::try_encode_nonstandard`'s doc)?
///
/// A loose mirror of `pack28`'s real acceptance shape — length 3-6 after
/// stripping a bare `/P` or `/R` suffix, and no OTHER `/` component —
/// without depending on pancetta-ft8 at runtime (see
/// `callsign_is_wire_representable`'s doc for why pancetta-qso can't call
/// `pack28` directly). Deliberately conservative in the SAFE direction:
/// this can return `true` for a callsign that would actually fail
/// `pack28`'s finer digit-position rules (e.g. `"8G81PA"` — 6 chars, no
/// other `/`, but position 3 is a digit where `pack28` requires a letter)
/// — an accepted false positive: it only costs speed (the QSO falls back
/// to the slower pre-existing generic timeout instead of this fast
/// watchdog check), never wrongly retires a QSO with a genuinely standard
/// callsign. It must NEVER return `false` for a callsign `pack28` would
/// actually accept.
fn callsign_is_plausibly_pack28_standard(callsign: &str) -> bool {
    let upper = callsign.to_ascii_uppercase();
    let base = upper
        .strip_suffix("/P")
        .or_else(|| upper.strip_suffix("/R"))
        .unwrap_or(upper.as_str());
    // Any OTHER '/' (a compound prefix, or a portable suffix other than a
    // bare /P or /R — pack28's ONLY two special-cased suffixes) is
    // definitely not pack28-representable.
    !base.contains('/') && (3..=6).contains(&base.len())
}

/// PAN-17 round 3 (Codex re-review of #248, finding 1): the DX callsign a
/// QSO's outgoing message would embed, ONLY for the rungs whose associated
/// message is a genuine numeric dB report/ack (`SignalReport`/`ReportAck`)
/// — never just a token (RRR/RR73/73) or a droppable grid. `None` for every
/// other state (nothing report-shaped to check).
///
/// `SendingReport`/`WaitingForReport` always transmit or re-send our own
/// numeric `SignalReport`/`ReportAck`. `WaitingForConfirmation` is
/// deliberately excluded (PAN-27 finding 3, round-4 review): the message it
/// currently has queued is always `FinalConfirmation` (RR73-class,
/// representable via i3=4 regardless of callsign shape) — a numeric
/// report/ack only re-enters the picture via a state REGRESSION back to
/// `SendingReport`, which changes `progress.state`'s variant itself, so the
/// `SendingReport` arm above already catches it once that happens. Checking
/// WaitingForConfirmation here as well falsely retired QSOs on the
/// skip-rung `RespondingToCq + ReportAck → WaitingForConfirmation` path,
/// which was about to complete cleanly with RR73. `SendingConfirmation` is
/// likewise excluded — it only ever sends RR73/73.
fn report_stage_partner_callsign(state: &QsoState) -> Option<&str> {
    match state {
        QsoState::SendingReport { their_callsign, .. }
        | QsoState::WaitingForReport { their_callsign, .. } => Some(their_callsign.as_str()),
        _ => None,
    }
}

/// Pick an audio offset in `[lo, hi]` deterministically from `seed` (e.g. a
/// callsign), spreading distinct seeds across the region so concurrent Hound
/// QSOs don't stack on one offset. Deterministic: same seed → same offset.
pub(crate) fn hound_offset_for(seed: &str, lo: f64, hi: f64) -> f64 {
    // Simple stable hash (FNV-1a style) → fraction of the range, snapped to 5 Hz.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let span = (hi - lo).max(0.0);
    if span == 0.0 {
        return lo;
    }
    let frac = (h % 10_000) as f64 / 10_000.0; // [0,1)
    let raw = lo + frac * span;
    // snap to 5 Hz, clamp inside [lo, hi]
    ((raw / 5.0).round() * 5.0).clamp(lo, hi)
}

/// Minimum TX audio separation (Hz) between concurrent QSOs to avoid
/// self-interference. At ≥75 Hz two FT8 signals (each ~50 Hz wide) do not
/// overlap; this matches criterion #7 in `SmartFrequencyAllocator`.
pub const MIN_TX_SEPARATION_HZ: f64 = 75.0;

/// Tight frequency-match tolerance (Hz) for INITIAL / ambiguous matching
/// (`CallingCq`, `Idle`, and any non-matching message), used by
/// [`QsoManager::is_message_relevant`], [`QsoManager::classify_relevance`],
/// and [`QsoManager::maybe_confirm_frequency_drift_at`]. Frequency tolerance
/// tightened from 50 Hz → 15 Hz to reduce cross-QSO message bleed-through in
/// multi-QSO mode: FT8 frame-to-frame drift is typically < 6 Hz on a stable
/// transceiver, so 15 Hz covers normal operation while shrinking the window
/// an attacker can exploit. (Security review 2026-04-29 C-1.)
///
/// These three sites must agree on the same tolerances — this constant (and
/// [`ESTABLISHED_FREQ_TOLERANCE_HZ`]) is the single shared definition so a
/// future tolerance change can't silently desync one site from the others
/// (PAN-15 item 6).
const FREQ_TOLERANCE_HZ: f64 = 15.0;

/// Wide frequency-match tolerance (Hz) once a QSO is ESTABLISHED (we know the
/// contra callsign and are past `CallingCq`/`Idle`) — an actively-answering
/// DX that has drifted beyond [`FREQ_TOLERANCE_HZ`] is not dropped. See
/// [`FREQ_TOLERANCE_HZ`] for the shared-definition rationale (PAN-15 item 6).
const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;

/// How long the offset a QSO just moved away from
/// ([`crate::states::QsoMetadata::pre_switch_offset`]) stays a valid RX
/// baseline, and stays the offset a forward advance is credited to.
///
/// PAN-72 (Codex round 4 on PR #350, finding 3). Two FT8 slots. The frame that
/// TRIGGERS a relocation is emitted on the old offset by the very
/// [`QsoManager::rearm_manual_calls_at`] pass that trips the stall threshold,
/// and the coordinator's once-per-slot drain commits the move only afterwards —
/// so the last pre-switch transmission is at most one slot old when the QSO
/// changes offset, its answer occupies the next slot, and that answer is
/// decoded and processed by the end of it. Two slots covers exactly that
/// round trip. It deliberately stops short of the ~3 slots it would take for an
/// answer to our first POST-switch transmission to arrive, so the two can never
/// be confused for one another.
const PRE_SWITCH_OFFSET_GRACE: chrono::Duration = chrono::Duration::seconds(30);

/// The offset this QSO moved away from, if it did so recently enough for a
/// reply to the last frame we sent there to still be arriving — see
/// [`PRE_SWITCH_OFFSET_GRACE`] and
/// [`crate::states::QsoMetadata::pre_switch_offset`].
fn live_pre_switch_offset_at(
    metadata: &crate::states::QsoMetadata,
    now: DateTime<Utc>,
) -> Option<f64> {
    metadata
        .pre_switch_offset
        .filter(|(_, left_at)| now.signed_duration_since(*left_at) <= PRE_SWITCH_OFFSET_GRACE)
        .map(|(hz, _)| hz)
}

/// FIX B (one-QSO-per-(callsign,band) close idempotency,
/// docs/qso-tx-deep-review-2026-07-18.md): how long after a manual QSO
/// COMPLETES a subsequent close-step (`Rr73`/`SeventyThree`) reply for the
/// same (callsign, band) is treated as "the operator is still trying to
/// close out the same contact" (re-key the existing QSO's last frame) vs. a
/// genuinely new contact (open a fresh QSO). See
/// [`QsoManager::find_recently_completed_manual_qso_for`]. Shared with the
/// coordinator's drop-stale-TX purge delay
/// (`pancetta::coordinator::qso`'s `completed_tx_grace`, formerly a separate
/// hardcoded 45s literal) so the two windows can never drift apart: it would
/// be incoherent for the QSO engine to still treat a completed QSO as
/// "reworkable" after the coordinator has already purged its TX liveness, or
/// vice versa.
pub const COMPLETED_QSO_REWORK_GRACE: chrono::Duration = chrono::Duration::seconds(45);

/// Nudge a candidate TX audio offset away from already-occupied offsets so
/// concurrent QSOs don't stack. Returns `candidate` unchanged if it is within
/// `[lo, hi]` AND at least `min_sep` Hz from every offset in `occupied`.
/// Otherwise searches outward from `candidate` in 25 Hz steps for the nearest
/// in-range offset that is `>= min_sep` from all `occupied`; if none exists in
/// range, returns `candidate.clamp(lo, hi)`. Deterministic (no RNG) — for tests
/// and reproducibility.
pub fn deconflict_offset(candidate: f64, occupied: &[f64], min_sep: f64, lo: f64, hi: f64) -> f64 {
    let clear = |off: f64| occupied.iter().all(|o| (off - o).abs() >= min_sep);
    let c = candidate.clamp(lo, hi);
    if clear(c) {
        return c;
    }
    // Search outward in 25 Hz steps: candidate±25, ±50, ... preferring the
    // closer side; only accept in-range offsets that clear all occupied.
    let step = 25.0;
    let max_steps = (((hi - lo) / step).ceil() as i64).max(1);
    for k in 1..=max_steps {
        let d = k as f64 * step;
        for cand in [c - d, c + d] {
            if cand >= lo && cand <= hi && clear(cand) {
                return cand;
            }
        }
    }
    c // no clear slot in range — fall back to the clamped candidate
}

/// The dial the station actually transmits on: the split TX dial when split is
/// active (`split_tx_hz != 0`), otherwise the RX dial. Used to stamp the logged
/// RF frequency of a completed QSO (dial + audio offset).
pub fn effective_tx_dial(rx_dial_hz: u64, split_tx_hz: u64) -> u64 {
    if split_tx_hz != 0 {
        split_tx_hz
    } else {
        rx_dial_hz
    }
}

/// QSO management errors
#[derive(Debug, Error)]
pub enum QsoManagerError {
    #[error("QSO not found: {qso_id}")]
    QsoNotFound { qso_id: QsoId },

    #[error("Invalid state transition from {from:?} to {to:?}")]
    InvalidTransition { from: QsoState, to: QsoState },

    #[error("QSO already exists for callsign {callsign} on frequency {frequency}")]
    DuplicateQso { callsign: String, frequency: f64 },

    #[error("Invalid callsign format: {callsign}")]
    InvalidCallsign { callsign: String },

    #[error("QSO timeout: {reason}")]
    Timeout { reason: String },

    #[error("Configuration error: {message}")]
    Configuration { message: String },

    #[error("Database error: {source}")]
    Database { source: anyhow::Error },

    #[error("{message}")]
    Internal { message: String },

    /// PAN-72 (Codex round 1 on PR #350, finding 3): a queued TX-offset
    /// action was drained after the QSO went terminal. Completed QSOs are
    /// retained in the map (and deliberately kept in the coordinator's active
    /// snapshots for a 45-second trailing-73 grace window), so the lookup
    /// still succeeds — but nothing may be mutated. Expected, not a fault.
    #[error("QSO {qso_id} is no longer active — TX-offset action discarded")]
    QsoNotActive { qso_id: QsoId },

    /// PAN-72 (Codex round 1 on PR #350, finding 8): a queued TX-offset
    /// action was drained after the QSO made forward progress, so the offset
    /// it wanted to move away from is the one that just demonstrably worked.
    /// Expected (the drain runs only once per 15-second slot), not a fault.
    #[error(
        "TX-offset action for QSO {qso_id} is stale \
         (raised at advance generation {raised_at}, QSO is now at {current})"
    )]
    OffsetActionStale {
        qso_id: QsoId,
        raised_at: u32,
        current: u32,
    },

    /// PAN-79 / PAN-72 (Codex round 3 on PR #350, finding 2): the resolved
    /// offset is the one the QSO is already on, so there is nothing to
    /// relocate. The allocator returns `avoid_hz` unchanged when every
    /// candidate is excluded (its documented "no valid relocation" signal) and
    /// the drain passes that straight through; committing it as a move would
    /// clear valid accumulated `stall_cycles` evidence and announce a
    /// `TxOffsetApplied` for a move that never happened. Expected, not a fault.
    #[error(
        "TX-offset action for QSO {qso_id} resolves to its current offset \
         ({offset_hz:.0} Hz) — nothing to relocate"
    )]
    OffsetActionNoOp { qso_id: QsoId, offset_hz: f64 },

    /// PAN-72 (Codex round 4 on PR #350, finding 2): the resolved offset falls
    /// outside the Hound region this QSO is procedurally pinned to *right now*.
    /// The coordinator resolves against a `get_qso` snapshot taken before the
    /// allocator runs, and the Fox's first report performs the mandatory
    /// calling-region → response-region QSY; if that lands in between, the
    /// resolved (low, calling-region) offset would undo the QSY and put our
    /// R+report where the Fox is not listening. Revalidated under the same
    /// write lock that performs the mutation, so check and commit are atomic.
    /// Expected, not a fault — the stall detector re-raises against the new
    /// region on its own.
    #[error(
        "TX-offset action for QSO {qso_id} resolves to {offset_hz:.0} Hz, \
         outside the Hound region it is now pinned to ({min_hz:.0}-{max_hz:.0} Hz)"
    )]
    OffsetActionOutsideHoundRegion {
        qso_id: QsoId,
        offset_hz: f64,
        min_hz: f64,
        max_hz: f64,
    },
}

impl QsoManagerError {
    /// PAN-72: is this an *expected* refusal of a queued TX-offset action
    /// (the QSO finished, or advanced, between the action being raised and
    /// the once-per-slot drain committing it, or the allocator found no
    /// candidate to relocate to at all) rather than a real fault? The
    /// coordinator's drain logs these at `debug!`/`info!` instead of `warn!`.
    pub fn is_expected_offset_action_refusal(&self) -> bool {
        matches!(
            self,
            QsoManagerError::QsoNotFound { .. }
                | QsoManagerError::QsoNotActive { .. }
                | QsoManagerError::OffsetActionStale { .. }
                | QsoManagerError::OffsetActionNoOp { .. }
                | QsoManagerError::OffsetActionOutsideHoundRegion { .. }
        )
    }
}

/// The Hz window a TX-offset action for this QSO is allowed to resolve inside,
/// or `None` for the general allocation range.
///
/// PAN-72 (Codex round 2 on PR #350, finding 1; hoisted here in round 4,
/// finding 2). A **Hound**'s TX offset is procedurally pinned, not free: while
/// calling the Fox we must sit in the low calling region, and once the Fox has
/// answered and we have QSY'd we must sit in the response region — that is
/// where the Fox is listening, and nowhere else. The generic allocator knows
/// nothing about either region, so a stall switch resolved through it could
/// move a post-QSY Hound back down into the calling region and guarantee the
/// QSO dies.
///
/// [`QsoMetadata::hound_qsyed`](crate::states::QsoMetadata::hound_qsyed) is the
/// discriminator rather than the QSO's state, because it is exactly the flag
/// the QSY itself sets (`process_message_for_qso`'s Hound block): before it
/// flips we are calling low, after it flips we are answering high. Non-Hound
/// QSOs are unconstrained.
///
/// Lives in this crate, not in the coordinator, so the constraint the
/// coordinator *resolves* against and the one
/// [`QsoManager::apply_tx_offset_switch`] *revalidates* at commit time are one
/// definition rather than two that can drift apart.
///
/// The window is sanitized rather than trusted: the bounds ultimately come from
/// operator TOML (`[hound]`), so a non-finite or inverted window degrades to
/// "no constraint" instead of filtering every candidate away (or, at the commit
/// end, refusing every action).
pub fn hound_switch_range_hz(
    progress: &crate::states::QsoProgress,
    config: &QsoManagerConfig,
) -> Option<(f64, f64)> {
    hound_switch_range_for(&progress.metadata, config)
}

/// [`hound_switch_range_hz`] against a bare metadata borrow — what
/// [`QsoManager::apply_tx_offset_switch`] has in hand while it holds the write
/// lock on the QSO it is about to mutate.
fn hound_switch_range_for(
    metadata: &crate::states::QsoMetadata,
    config: &QsoManagerConfig,
) -> Option<(f64, f64)> {
    if !metadata.hound {
        return None;
    }
    let (lo, hi) = if metadata.hound_qsyed {
        (config.hound.response_min_hz, config.hound.response_max_hz)
    } else {
        (config.hound.call_min_hz, config.hound.call_max_hz)
    };
    (lo.is_finite() && hi.is_finite() && lo <= hi).then_some((lo, hi))
}

/// QSO manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoManagerConfig {
    /// Our station callsign
    pub our_callsign: String,

    /// Our grid square
    pub our_grid: Option<GridSquare>,

    /// Timeout settings
    pub timeouts: TimeoutConfig,

    /// Contest mode settings
    pub contest_mode: Option<ContestConfig>,

    /// Automatic sequencing configuration
    pub auto_sequence: AutoSequenceConfig,

    /// Duplicate checking settings
    pub duplicate_checking: DuplicateCheckConfig,

    /// Hound (DXpedition chaser) audio-offset regions.
    ///
    /// Controls the calling region (low) and the post-QSY response region (high).
    /// Defaults to 300–900 Hz (call) / 1000–2700 Hz (response), matching the
    /// WSJT-X Fox/Hound conventions.  Populate from `pancetta_config::HoundConfig`
    /// in the coordinator to honour operator-set ranges.
    #[serde(default)]
    pub hound: HoundRegions,

    /// Station-wide active operating mode string (`"FT8"`, `"FT4"`, or
    /// `"FT2"`), stamped into every [`QsoMetadata::mode`] this manager
    /// creates and thus into the ADIF `MODE` field of logged QSOs. FT8 is a
    /// station-global mode (not per-decode); the coordinator populates this
    /// from `[rig].mode`. Defaults to `"FT8"` so the legacy path is
    /// byte-identical.
    #[serde(default = "default_active_mode")]
    pub active_mode: String,
}

/// Default value for [`QsoManagerConfig::active_mode`]: `"FT8"`.
fn default_active_mode() -> String {
    "FT8".to_string()
}

/// Timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Timeout for CQ calls (seconds)
    pub cq_timeout: u64,

    /// Timeout for waiting for report (seconds)
    pub report_timeout: u64,

    /// Timeout for waiting for confirmation (seconds)
    pub confirmation_timeout: u64,

    /// Maximum QSO duration (seconds)
    pub max_qso_duration: u64,

    /// Cleanup interval for completed QSOs (seconds)
    pub cleanup_interval: u64,

    /// Manual keep-calling watchdog: stop calling after this many minutes
    /// have elapsed since the first manual call, regardless of call count.
    /// Whichever of this and `manual_call_max_calls` fires first ends the
    /// manual call attempt. Default: 5 minutes.
    pub manual_call_watchdog_minutes: u64,

    /// Manual keep-calling watchdog: stop after transmitting this many
    /// calls to the DX. Whichever of this and `manual_call_watchdog_minutes`
    /// fires first ends the manual call attempt. Default: 25 — high enough that
    /// the 5-minute time watchdog is the governing limit (operator wants ~5 min
    /// of calling, not the old ~2.5 min the 10-call cap produced); it remains a
    /// safety backstop. Applies only to the INITIAL call states.
    pub manual_call_max_calls: u32,

    /// Repetitive-TX watchdog (seconds). If a QSO sits in the SAME active TX
    /// state this long — i.e. we have been re-sending the same message without
    /// the DX advancing us — the QSO is retired as a timeout. Bounds "stuck
    /// sending the same thing" for BOTH manual and auto QSOs, independent of
    /// (and tighter than) the manual keep-call watchdog. Default: 300 s.
    /// (Raised from 120 s on operator request — we were giving up on calling a
    /// non-answering DX too quickly. The 5-min keep-call watchdog now governs.)
    #[serde(default = "default_repetitive_tx_timeout_secs")]
    pub repetitive_tx_timeout_secs: u64,

    /// Consecutive stalled cycles (see [`crate::states::QsoMetadata::
    /// stall_cycles`]) before an in-progress QSO's TX offset is switched (or
    /// reverted to its last known-good offset) — Auto TX-freq mode only.
    /// Default 4 (PAN-72). Distinct from `AutonomousConfig::
    /// cq_no_response_switch_after` (a different struct, governs the
    /// self-CQ-hunting case before any QSO exists), though the two are
    /// exposed under the same `[autonomous]` TOML section by the coordinator
    /// (see `pancetta-config`) since they're one logical "adaptive TX
    /// behavior" knob from the operator's point of view.
    #[serde(default = "default_qso_stall_switch_after")]
    pub qso_stall_switch_after: u32,
}

/// Default for [`TimeoutConfig::repetitive_tx_timeout_secs`] (5 minutes).
fn default_repetitive_tx_timeout_secs() -> u64 {
    300
}

/// Default for [`TimeoutConfig::qso_stall_switch_after`] (PAN-72).
fn default_qso_stall_switch_after() -> u32 {
    4
}

/// Contest configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestConfig {
    /// Contest name
    pub contest_name: String,

    /// Contest category
    pub category: String,

    /// Starting serial number
    pub starting_serial: SerialNumber,

    /// Enable contest mode
    pub enabled: bool,
}

/// Automatic sequencing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSequenceConfig {
    /// Enable automatic sequencing
    pub enabled: bool,

    /// Automatically respond to CQ calls
    pub auto_respond_cq: bool,

    /// Automatically send reports
    pub auto_send_reports: bool,

    /// Automatically send confirmations
    pub auto_send_confirmations: bool,

    /// Delay between automatic actions (milliseconds)
    pub action_delay_ms: u64,
}

/// Duplicate checking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateCheckConfig {
    /// Enable duplicate checking
    pub enabled: bool,

    /// Check duplicates within this time window (hours)
    pub time_window_hours: u32,

    /// Check duplicates on same frequency
    pub check_frequency: bool,

    /// Check duplicates on same band
    pub check_band: bool,
}

/// What an in-progress QSO's stall-detection (PAN-72) wants done about its
/// TX offset. Emitted by `QsoManager` as `QsoEvent::TxOffsetActionNeeded`;
/// resolved and committed by the coordinator (see
/// `docs/superpowers/specs/2026-09-04-pan-72-adaptive-tx-offset-design.md`)
/// since only `AutonomousOperator` may call the smart allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OffsetAction {
    /// We're on the last known-good offset (or none is recorded yet) — find
    /// a new one, avoiding this one.
    Switch { avoid_hz: f64 },
    /// We stalled again on a previously-switched offset — go back to the one
    /// that was last confirmed working, no allocator call needed.
    Revert { target_hz: f64 },
}

/// PAN-72 (Codex round 1 on PR #350, finding 8): one queued TX-offset action
/// plus the staleness token that guards it at commit time.
///
/// This is the element type of the coordinator's `pending_qso_offset_requests`
/// mailbox. It lives here, next to [`OffsetAction`], because
/// [`QsoManager::apply_tx_offset_switch`] is what actually validates the
/// token — the coordinator only carries it across the once-per-slot gap
/// between the QSO event firing and the autonomous tick draining it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffsetActionRequest {
    pub qso_id: QsoId,
    pub action: OffsetAction,
    /// [`crate::states::QsoMetadata::advance_generation`] as it stood when
    /// this action was raised. `None` for an operator-forced `u` nudge, which
    /// is by definition current and must never be discarded as stale.
    pub raised_at_generation: Option<u32>,
}

impl OffsetActionRequest {
    /// A stall-detected action, guarded by the QSO's advance generation.
    pub fn stall_detected(qso_id: QsoId, action: OffsetAction, raised_at_generation: u32) -> Self {
        Self {
            qso_id,
            action,
            raised_at_generation: Some(raised_at_generation),
        }
    }

    /// An operator-forced action (the `u` nudge) — never stale.
    pub fn operator_forced(qso_id: QsoId, action: OffsetAction) -> Self {
        Self {
            qso_id,
            action,
            raised_at_generation: None,
        }
    }
}

/// QSO event notifications
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QsoEvent {
    /// QSO state changed
    StateChanged {
        qso_id: QsoId,
        old_state: QsoState,
        new_state: QsoState,
        timestamp: DateTime<Utc>,
    },

    /// Message received
    MessageReceived { qso_id: QsoId, message: QsoMessage },

    /// Message should be sent
    MessageToSend {
        qso_id: QsoId,
        message: MessageType,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        /// SECURITY: `true` iff the emitting QSO's `QsoMetadata.remote_origin`
        /// is set (the QSO was initiated by a remote operator). The coordinator
        /// forwards this as `TxOrigin::Remote` so the frame is armed-TX gated.
        /// `false` for every Local / TUI / autonomous QSO.
        remote_origin: bool,
    },

    /// QSO completed
    QsoCompleted {
        qso_id: QsoId,
        metadata: QsoMetadata,
        /// Full state-transition + message timeline as of completion (Layer
        /// 2 timeline persistence — see
        /// docs/observability-diagnostics-plan.md). Captured at the
        /// emission site from the live in-memory `QsoProgress` before it is
        /// eventually dropped (`cleanup_completed_qsos` removes terminal
        /// QSOs from the active map ~1h later, discarding these fields if
        /// nothing persisted them first). Consumers that don't need the
        /// timeline can destructure with `..`.
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    },

    /// QSO failed
    QsoFailed {
        qso_id: QsoId,
        reason: QsoFailureReason,
        /// Operational state immediately before the QSO entered `Failed`.
        /// Unlike `state_history`, this is populated even when a newly opened
        /// QSO times out before its first sent/received transition.
        last_state: QsoState,
        metadata: QsoMetadata,
        /// See `QsoCompleted::state_history`.
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    },

    /// Duplicate QSO detected
    DuplicateDetected {
        qso_id: QsoId,
        original_qso_id: QsoId,
        callsign: String,
    },

    /// A decoded frame was refused on sender-verification grounds.
    MessageRejected {
        qso_id: QsoId,
        reason: RejectionReason,
        from_callsign: Option<String>,
        to_callsign: Option<String>,
    },

    /// PAN-72: an in-progress QSO's TX offset needs an autonomous action
    /// (Auto TX-freq mode only — see `QsoManager::rearm_manual_calls_at`).
    /// The coordinator resolves `OffsetAction::Switch` via the smart
    /// allocator and commits either variant via
    /// `QsoManager::apply_tx_offset_switch`.
    ///
    /// `raised_at_generation` is the QSO's
    /// [`crate::states::QsoMetadata::advance_generation`] at emission time —
    /// carried through the coordinator's mailbox and re-checked at commit
    /// time so an action raised before a DX advance that has since landed is
    /// discarded rather than moving the QSO off the offset that just worked.
    TxOffsetActionNeeded {
        qso_id: QsoId,
        action: OffsetAction,
        raised_at_generation: u32,
    },

    /// PAN-72: a TX-offset action was actually committed by
    /// [`QsoManager::apply_tx_offset_switch`] (post-clamp value). Purely an
    /// announcement — nothing in `pancetta-qso` consumes it. The coordinator
    /// uses it to rebuild the `ActiveQsosSnapshot` the TUI banner and the
    /// remote gateway render from, which is otherwise rebuilt only on a state
    /// transition; a stalled exchange may produce none for a long time, so
    /// without this the displayed offset would lag through multiple
    /// switch/revert cycles (Codex round 1 on PR #350, finding 7).
    TxOffsetApplied { qso_id: QsoId, offset_hz: f64 },
}

/// Routing verdict plus an optional security classification.
struct Relevance {
    relevant: bool,
    reason: Option<RejectionReason>,
}

impl Default for QsoManagerConfig {
    fn default() -> Self {
        Self {
            our_callsign: "NOCALL".to_string(),
            our_grid: None,
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            cq_timeout: 30,
            report_timeout: 30,
            confirmation_timeout: 30,
            max_qso_duration: 300,
            cleanup_interval: 60,
            manual_call_watchdog_minutes: 5,
            manual_call_max_calls: 25,
            repetitive_tx_timeout_secs: default_repetitive_tx_timeout_secs(),
            qso_stall_switch_after: default_qso_stall_switch_after(),
        }
    }
}

impl Default for AutoSequenceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_respond_cq: false,
            auto_send_reports: false,
            auto_send_confirmations: false,
            action_delay_ms: 1000,
        }
    }
}

impl Default for DuplicateCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time_window_hours: 24,
            check_frequency: true,
            check_band: false,
        }
    }
}

/// QSO manager implementation
pub struct QsoManager {
    /// Configuration
    config: QsoManagerConfig,

    /// Active QSOs by ID
    qsos: Arc<RwLock<HashMap<QsoId, QsoProgress>>>,

    /// QSOs by callsign for duplicate checking
    qsos_by_callsign: Arc<RwLock<HashMap<String, Vec<QsoId>>>>,

    /// Event broadcaster
    event_sender: broadcast::Sender<QsoEvent>,

    /// Next contest serial number
    next_serial: Arc<RwLock<SerialNumber>>,

    /// Cleanup interval timer
    cleanup_interval: Arc<RwLock<Option<Interval>>>,

    /// Optional database for persistent duplicate checking
    database: Option<Arc<QsoDatabase>>,

    /// Rig dial frequency in Hz, shared from the coordinator's hamlib poll
    /// (0 if unknown / no rig). `metadata.frequency` holds the *audio offset*;
    /// the logged RF frequency of a completed QSO is `dial + offset` (WSJT-X
    /// convention). Used only when stamping completed-QSO metadata so the ADIF
    /// records a real FREQ/BAND instead of the bare offset.
    dial_frequency_hz: Arc<AtomicU64>,

    /// Rig split-TX dial in Hz (0 = simplex), shared from the coordinator.
    /// When nonzero, completed-QSO RF is stamped against this TX dial instead
    /// of `dial_frequency_hz` (the RX dial). Defaults to a private `0` atomic
    /// so callers that never inject a source keep simplex (RX==TX) behavior.
    split_tx_frequency_hz: Arc<AtomicU64>,

    /// Operator TX-frequency mode (`pancetta_core::TxFreqMode` as `u8`), shared
    /// from the coordinator. The silence-driven stall detector (`stall_cycles`
    /// counted in `rearm_manual_calls_at`, emitting
    /// `QsoEvent::TxOffsetActionNeeded` at threshold) only fires in `Auto`
    /// mode; in the default `Hold` mode the operator's picked offset is sticky
    /// and never moved autonomously. Defaults to a private `Hold` atomic so
    /// unit tests and any caller that never injects a source keep the
    /// hold-the-frequency behavior.
    tx_freq_mode: Arc<std::sync::atomic::AtomicU8>,

    /// Global operator TX policy (`pancetta_core::TxPolicy` as `u8`), shared
    /// from the coordinator.
    ///
    /// PAN-72 (Codex round 2 on PR #350, finding 5). The silence-driven stall
    /// detector treats each per-slot re-send in `rearm_manual_calls_at` as
    /// evidence that the DX heard us and did not answer. Under
    /// `TxPolicy::Disabled` that inference is simply false: the coordinator's
    /// `tx_hard_mute_reason` blocks the transmission outright, so the DX's
    /// silence says nothing at all — yet the keep-calling loop still re-arms
    /// every slot. Left uncounted... it was counted, and four muted cycles in
    /// `TxFreqMode::Auto` moved an established QSO off a known-good offset
    /// nothing had actually transmitted on.
    ///
    /// Defaults to a private `Full` atomic so unit tests and any caller that
    /// never injects a source keep the pre-existing behavior (TX assumed
    /// live). `Full` and `RespondOnly` both `allows_any_tx()`; only `Disabled`
    /// suppresses the count. This covers the operator-visible mute only —
    /// `tx_hard_mute_reason`'s other causes (e.g. `restart_inhibit` during a
    /// Hamlib supervisor restart, which AGENTS.md documents as spanning
    /// multiple slots) are not visible to `QsoManager` today and can still
    /// let a muted cycle count as a stall. Narrower than "the hard mute" as a
    /// whole; PAN follow-up filed to thread the full predicate through.
    tx_policy: Arc<std::sync::atomic::AtomicU8>,
}

/// Outcome of the half-duplex parity-admission check for a *new* QSO.
///
/// FT8 is half-duplex on a 15 s slot grid: while we transmit in one window we
/// are deaf to the band. To keep the *opposite* window always free for hearing
/// responses, **every concurrent active QSO must transmit on the same parity**
/// (the "TX side"). A new QSO whose desired `tx_parity` matches the current
/// side (or any QSO when we are idle) is [`TxAdmission::Admit`]ted and runs
/// concurrently; a cross-parity request is [`TxAdmission::Queue`]d until all
/// current-side QSOs finish and a clean window flip is possible. We never
/// preempt an in-flight QSO to change sides. See [`admit_new_qso`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAdmission {
    /// Start the QSO now — either we are idle (it adopts its own side) or its
    /// desired parity matches the current TX side (concurrent, same window).
    Admit,
    /// Defer the QSO — it wants the opposite window from the one we are
    /// currently committed to. Hold it until the current-side QSOs complete.
    Queue,
}

/// Pure half-duplex admission decision (see [`TxAdmission`]).
///
/// * `current_side` — the parity all currently-active QSOs transmit on, or
///   `None` when we are idle (no committed side).
/// * `desired` — the parity this new QSO wants to transmit on (`None` means the
///   caller hasn't pinned a parity, e.g. a CQ that lets the scheduler choose —
///   such a request never conflicts and is always admitted, adopting whatever
///   side is live).
///
/// Rules: idle → admit (adopt the side); unpinned desire → admit (flexible);
/// same side → admit (concurrent, same window); cross-side → queue.
pub fn admit_new_qso(
    current_side: Option<pancetta_core::slot::SlotParity>,
    desired: Option<pancetta_core::slot::SlotParity>,
) -> TxAdmission {
    match (current_side, desired) {
        // Idle: the new QSO defines the side.
        (None, _) => TxAdmission::Admit,
        // Caller didn't pin a parity: it can ride the live side.
        (Some(_), None) => TxAdmission::Admit,
        // Committed side and a pinned desire: admit iff they match.
        (Some(side), Some(want)) => {
            if side == want {
                TxAdmission::Admit
            } else {
                TxAdmission::Queue
            }
        }
    }
}

impl QsoManager {
    /// Current true RF frequency for an audio-offset frequency, using the
    /// live rig dial (+ split TX dial if active) -- the same `effective_tx_dial`
    /// pattern used to stamp a Completed QSO's `metadata.frequency` (PAN-25).
    /// Falls back to the bare offset when no dial source is available
    /// (`dial == 0`, e.g. unit tests / no rig), matching that same stamping
    /// fallback exactly, so a completed QSO whose frequency was never
    /// dial-adjusted still compares consistently against a same-session
    /// incoming (also un-adjusted) frequency.
    fn current_rf_frequency(&self, audio_offset_hz: f64) -> f64 {
        let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
        let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
        let dial = effective_tx_dial(rx_dial, split);
        if dial > 0 {
            audio_offset_hz + dial as f64
        } else {
            audio_offset_hz
        }
    }

    /// The parity our currently-active QSOs are committed to transmitting on,
    /// or `None` when idle.
    ///
    /// Half-duplex discipline (see [`admit_new_qso`]) guarantees every active
    /// QSO shares one TX parity, so the first active QSO carrying a latched
    /// `tx_parity` defines the side. QSOs with `tx_parity == None` (parity left
    /// to the scheduler) don't pin a side and are skipped. Returns `None` when
    /// no active QSO pins a parity (we are free to adopt either window).
    pub async fn current_tx_side(&self) -> Option<pancetta_core::slot::SlotParity> {
        let qsos = self.qsos.read().await;
        qsos.values()
            .filter(|p| p.state.is_active())
            .find_map(|p| p.metadata.tx_parity)
    }

    /// Count of currently-active (non-terminal) QSOs.
    pub async fn active_qso_count(&self) -> usize {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| p.state.is_active())
            .count()
    }

    /// Count of currently-active (non-terminal) QSOs that are **not** in the
    /// `CallingCq` state — i.e. caller-answer / in-exchange QSOs only.
    ///
    /// Used by the Fox-mode path in `maybe_answer_caller`: the Fox's own CQ
    /// QSO is `CallingCq` and must not eat a Hound-answer slot.  With default
    /// `fox_max_streams = 5`, this lets 5 Hounds be worked simultaneously
    /// while the CQ keeps running (CQ + 5 answers = 6 streams ≤
    /// `MAX_RETAINED_TX_STREAMS = 8`).
    pub async fn active_caller_qso_count(&self) -> usize {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| {
                p.state.is_active() && !matches!(p.state, crate::states::QsoState::CallingCq { .. })
            })
            .count()
    }

    /// Whether any *active* (non-terminal) QSO already exists with `callsign`
    /// (compound-call-aware via [`crate::exchange::callsigns_match`], so
    /// `EA8/G8BCG` and `G8BCG` count as the same station). Used by the
    /// always-answer-callers path to avoid opening a duplicate QSO with a
    /// station an exchange is already in progress with.
    pub async fn has_active_qso_with(&self, callsign: &str) -> bool {
        let qsos = self.qsos.read().await;
        qsos.values().any(|p| {
            p.state.is_active()
                && p.metadata
                    .their_callsign
                    .as_deref()
                    .is_some_and(|c| crate::exchange::callsigns_match(c, callsign))
        })
    }

    /// True if we have an ACTIVE QSO with `callsign`, OR a recently-COMPLETED
    /// one on the SAME band (within `within`). Used to suppress the
    /// always-answer-callers path from opening a second QSO for a station we
    /// just finished working — the bounded auto-resend-73 path already
    /// handles a DX that didn't copy our 73. Explicit operator re-work
    /// bypasses this gate entirely (it uses the `StartQso` →
    /// `respond_to_cq_manual` path, not `maybe_answer_caller`).
    ///
    /// Compound-call-aware: `EA8/G8BCG` and `G8BCG` are the same station.
    ///
    /// PAN-25 round 2 (Codex): `frequency` (the incoming message's audio
    /// offset) band-scopes the completed-QSO arm the same way
    /// `find_qsos_for_message`/`find_recently_completed_manual_qso_for_at`
    /// already do — without it, a station worked on 20m stayed "reserved"
    /// against this preflight for a fresh direct call on 40m too.
    pub async fn has_active_or_recent_qso_with(
        &self,
        callsign: &str,
        frequency: f64,
        within: std::time::Duration,
    ) -> bool {
        let qsos = self.qsos.read().await;
        let now = chrono::Utc::now();
        let window_secs = within.as_secs() as i64;
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        qsos.values().any(|p| {
            let call_match = p
                .metadata
                .their_callsign
                .as_deref()
                .is_some_and(|c| crate::exchange::callsigns_match(c, callsign));
            if !call_match {
                return false;
            }
            if p.state.is_active() {
                return true;
            }
            // Recently completed, on the same band? (`completed_at` lives in
            // QsoState::Completed { .. }.)
            if let QsoState::Completed { completed_at, .. } = &p.state {
                let completed_band = crate::utils::frequency_to_band(
                    p.metadata
                        .completed_rf_frequency_hz
                        .unwrap_or(p.metadata.frequency),
                );
                return completed_band == want_band
                    && now.signed_duration_since(*completed_at).num_seconds() <= window_secs;
            }
            false
        })
    }

    /// Create a new QSO manager
    pub fn new(config: QsoManagerConfig) -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        let next_serial = config
            .contest_mode
            .as_ref()
            .map(|c| c.starting_serial)
            .unwrap_or(1);

        Self {
            config,
            qsos: Arc::new(RwLock::new(HashMap::new())),
            qsos_by_callsign: Arc::new(RwLock::new(HashMap::new())),
            event_sender,
            next_serial: Arc::new(RwLock::new(next_serial)),
            cleanup_interval: Arc::new(RwLock::new(None)),
            database: None,
            dial_frequency_hz: Arc::new(AtomicU64::new(0)),
            split_tx_frequency_hz: Arc::new(AtomicU64::new(0)),
            // Default Hold: the silence-driven stall detector stays off unless
            // the coordinator injects a shared mode atomic and the operator
            // switches to Auto.
            tx_freq_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Hold.as_u8(),
            )),
            // Default Full: with no injected source, assume TX is live — the
            // pre-existing behavior (see the field's doc comment).
            tx_policy: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxPolicy::Full.as_u8(),
            )),
        }
    }

    /// Share the coordinator's rig dial-frequency source so completed QSOs log
    /// the true RF frequency (dial + audio offset) instead of the bare offset.
    /// Pass the same `Arc<AtomicU64>` the hamlib poll loop updates; if never
    /// called, completed metadata keeps the offset value (e.g. unit tests).
    pub fn set_dial_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.dial_frequency_hz = source;
    }

    /// Share the coordinator's split-TX dial source so completed QSOs log the
    /// real TX RF during split operation. Pass the same `Arc<AtomicU64>` the
    /// TUI SetSplit relay updates (0 = simplex). If never called, the manager
    /// keeps its private `0` (RX==TX).
    pub fn set_split_tx_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.split_tx_frequency_hz = source;
    }

    /// Share the coordinator's TX-frequency-mode atomic so the silence-driven
    /// stall detector (`stall_cycles` → `QsoEvent::TxOffsetActionNeeded`)
    /// respects the operator's Hold/Auto choice at runtime. Pass the same
    /// `Arc<AtomicU8>` the TUI toggle updates (encoded via
    /// [`pancetta_core::TxFreqMode::as_u8`]). If never called, the manager keeps
    /// its private `Hold` default (no autonomous frequency changes).
    pub fn set_tx_freq_mode_source(&mut self, source: Arc<std::sync::atomic::AtomicU8>) {
        self.tx_freq_mode = source;
    }

    /// Share the coordinator's global TX-policy atomic (encoded via
    /// [`pancetta_core::TxPolicy::as_u8`]) so the silence-driven stall detector
    /// can tell "the DX ignored us" apart from "we never actually transmitted".
    ///
    /// PAN-72 (Codex round 2 on PR #350, finding 5) — see the `tx_policy`
    /// field's doc comment. Mirrors [`Self::set_tx_freq_mode_source`]: pass the
    /// same `Arc<AtomicU8>` the TUI's policy toggle writes. If never called,
    /// the manager keeps its private `Full` default and behaves exactly as
    /// before.
    pub fn set_tx_policy_source(&mut self, source: Arc<std::sync::atomic::AtomicU8>) {
        self.tx_policy = source;
    }

    /// Create a new QSO manager with a database for persistent duplicate checking
    pub fn with_database(config: QsoManagerConfig, database: Arc<QsoDatabase>) -> Self {
        let mut manager = Self::new(config);
        manager.database = Some(database);
        manager
    }

    /// Get the configuration
    pub fn config(&self) -> &QsoManagerConfig {
        &self.config
    }

    /// Update the station-wide active mode string stamped into every
    /// [`QsoMetadata::mode`] this manager creates from now on. Only affects
    /// QSOs opened AFTER this call — anything already in progress keeps its
    /// already-stamped mode. Called from the coordinator's QSO task when the
    /// operator switches mode live (Shift+M); the caller (coordinator) is
    /// responsible for having already confirmed no QSO is active before the
    /// switch (see `try_switch_operating_mode`), so this setter itself does
    /// no gating.
    pub fn set_active_mode(&mut self, mode: String) {
        self.config.active_mode = mode;
    }

    /// Start the QSO manager
    pub async fn start(&self) -> Result<(), QsoManagerError> {
        info!("Starting QSO manager for {}", self.config.our_callsign);

        // Start cleanup timer
        let cleanup_duration = TokioDuration::from_secs(self.config.timeouts.cleanup_interval);
        let interval_timer = interval(cleanup_duration);
        *self.cleanup_interval.write().await = Some(interval_timer);

        // Start background tasks
        let manager = self.clone();
        tokio::spawn(async move {
            manager.cleanup_loop().await;
        });

        let manager = self.clone();
        tokio::spawn(async move {
            manager.timeout_check_loop().await;
        });

        Ok(())
    }

    /// Subscribe to QSO events
    pub fn subscribe(&self) -> broadcast::Receiver<QsoEvent> {
        self.event_sender.subscribe()
    }

    /// Resolve a CQ opening's TX parity to a CONCRETE value, latching it once.
    ///
    /// docs/qso-engine-bugs.md BUG 1: calling CQ transmitted on EVERY 15s
    /// window instead of alternating. Root cause: `start_cq`/`start_cq_manual`
    /// accepted `tx_parity: None` (the caller's "Auto, no fixed preference"
    /// case — the DEFAULT for both the manual and autonomous CQ paths) and
    /// stored `None` directly into `QsoMetadata.tx_parity`. Every subsequent
    /// emission (the opening CQ AND every per-slot keep-call rearm) then
    /// re-asked the TX scheduler's `resolve_required_parity(None, Auto, now,
    /// …)` to pick "the nearest next slot" FRESH each time — which is just
    /// "whichever window is next," not a fixed side. Over consecutive calls
    /// that alternates Even/Odd/Even/Odd… i.e. TRANSMITS ON BOTH PARITIES,
    /// so the opposite (reply) window is never free and we never hear anyone
    /// answering our own CQ.
    ///
    /// Fix: resolve `None` to a CONCRETE parity exactly ONCE, here, at QSO
    /// creation, using the same "nearest next slot of either parity" rule the
    /// TX scheduler's `Auto` mode uses (mirrored here with only
    /// `pancetta_core::slot` primitives, since this crate has no dependency
    /// on the coordinator's `TxSelfParity` type) — then that ONE resolved
    /// value is stored in `QsoMetadata.tx_parity` and reused, unchanged, by
    /// the opening CQ and every rearm for the life of the QSO. A caller that
    /// already supplied a fixed preference (`Some(Even)`/`Some(Odd)`, e.g. a
    /// station configured for a specific side) is untouched — this only
    /// resolves the previously-ambiguous `None` case.
    fn latch_cq_parity_if_none(
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        now: DateTime<Utc>,
    ) -> Option<pancetta_core::slot::SlotParity> {
        use pancetta_core::slot::{next_slot_with_parity, SlotParity};
        Some(tx_parity.unwrap_or_else(|| {
            let next_even = next_slot_with_parity(now, SlotParity::Even);
            let next_odd = next_slot_with_parity(now, SlotParity::Odd);
            if next_even <= next_odd {
                SlotParity::Even
            } else {
                SlotParity::Odd
            }
        }))
    }

    /// Start a new CQ call
    /// Start a CQ call.
    ///
    /// `tx_parity` is the parity we want our CQ to land on, if the caller has
    /// a fixed preference. `None` (no preference — the Auto-config default)
    /// is resolved to a CONCRETE parity ONCE at creation
    /// ([`Self::latch_cq_parity_if_none`]) and held for the life of the QSO —
    /// it does NOT mean "let the scheduler re-pick every slot" (that was BUG
    /// 1: re-picking "nearest slot" every call alternates both parities,
    /// i.e. transmits every window and never listens).
    pub async fn start_cq(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
    ) -> Result<QsoId, QsoManagerError> {
        self.start_cq_with_id(Uuid::new_v4(), frequency, tx_parity, remote_origin)
            .await
    }

    /// PAN-38 round 2 (Codex): same as [`Self::start_cq`], but lets the
    /// caller supply `qso_id` up front instead of having this function
    /// generate it internally. The autonomous coordinator uses this to
    /// register the qso_id<->cq_attempt_id association (`AutonomousCqOpened`)
    /// BEFORE this call's own `MessageToSend` becomes visible to the
    /// independently-scheduled event-forwarding task — otherwise a
    /// same-instant downstream failure's `TransmitComplete` could reach the
    /// autonomous task before the association does, silently dropping the
    /// rollback (and then registering a stale, never-cleaned-up entry when
    /// the late `AutonomousCqOpened` finally arrives).
    pub async fn start_cq_with_id(
        &self,
        qso_id: QsoId,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let now = Utc::now();
        // BUG 1 fix (docs/qso-engine-bugs.md): latch a concrete parity ONCE at
        // QSO creation when the caller has no fixed preference (`None`, the
        // Auto-config default) — see `latch_cq_parity_if_none` doc.
        let tx_parity = Self::latch_cq_parity_if_none(tx_parity, now);

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            // Calling CQ is not a manual keep-calling QSO; it has its own
            // CallingCq timeout and call_count in the state itself.
            initiated_by: CallInitiation::Auto,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin,
            // CQ-path latch: resolved from our own preference, not an
            // observed DX parity — always self-consistent, never provisional.
            tx_parity_provisional: false,
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit CQ message
        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;

        info!("Started CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Start a **manual** (operator-initiated) CQ call as a real QSO.
    ///
    /// This is the engine half of the TUI `c` (StartCq) key. Unlike
    /// [`Self::start_cq`] (which marks the QSO [`CallInitiation::Auto`] for
    /// autonomous CQ), this marks it [`CallInitiation::Manual`] so that:
    ///
    /// 1. **We keep calling CQ every slot** until a station answers or the
    ///    CQ watchdog fires — [`Self::rearm_manual_calls_at`] re-emits a
    ///    `Cq` `MessageToSend` for a manual `CallingCq` QSO once per FT8
    ///    slot, bounded by `manual_call_max_calls` /
    ///    `manual_call_watchdog_minutes` (see [`Self::check_timeouts_at`]).
    /// 2. **When a caller answers, the exchange auto-sequences to
    ///    Completed + logs** — the auto-reply emitter in
    ///    [`Self::process_message`] is gated on `CallInitiation::Manual`, so
    ///    a manual CQer (us) automatically replies with our report → RR73 as
    ///    the caller's CqResponse → ReportAck arrive, exactly like the
    ///    operator-driven Callers path.
    ///
    /// We emit a `StateChanged` (Idle → CallingCq) so the coordinator's
    /// drop-stale-TX gate keys this QSO into `active_tx_qsos` (otherwise the
    /// TX worker would refuse to key PTT for it), and emit the first `Cq`
    /// `MessageToSend` immediately. `last_call_at` is stamped `now` so the
    /// per-slot rearm does not double-send the first CQ within the opening
    /// slot.
    ///
    /// `tx_parity` is the parity we want our CQ to land on, if fixed; `None`
    /// (no preference) is resolved to a CONCRETE parity ONCE here
    /// ([`Self::latch_cq_parity_if_none`]) and held for the life of the QSO —
    /// see that function's doc for the BUG 1 (transmit-every-window) history.
    /// (Calling CQ, we choose our own slot parity — there is no DX parity to
    /// oppose.)
    pub async fn start_cq_manual(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        // BUG 1 fix (docs/qso-engine-bugs.md): latch a concrete parity ONCE at
        // QSO creation when the caller has no fixed preference (`None`, the
        // Auto-config default) — see `latch_cq_parity_if_none` doc.
        let tx_parity = Self::latch_cq_parity_if_none(tx_parity, now);

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        let raw_text = self.render_sent_text(&message);
        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            // Operator pressed `c`: this is a MANUAL CQ. The manual
            // keep-calling watchdog re-arms our CQ every slot, and the
            // CallInitiation::Manual gate turns on the auto-reply emitter so
            // an answering station drives the exchange to completion.
            initiated_by: CallInitiation::Manual,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            // `false` for operator-pressed `c` (local); `true` for a remote
            // operator's `startCq` routed via the station agent.
            remote_origin,
            // CQ-path latch: resolved from our own preference, not an
            // observed DX parity — always self-consistent, never provisional.
            tx_parity_provisional: false,
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            // Record the opening CQ as a Sent message so the TUI last-TX line
            // and `resend_last_tx` see it.
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit a state change (Idle → CallingCq) so the coordinator's
        // drop-stale-TX gate keys this QSO into `active_tx_qsos`; without it
        // the TX worker would drop our CQ as "stale TX for an ended QSO".
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        // Emit the first CQ. Subsequent slots are owned by the per-slot
        // manual keep-call rearm (`rearm_manual_calls_at`).
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;

        info!("Started manual CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Respond to a CQ call (autonomous/internal path).
    ///
    /// `dx_parity` is the slot parity of the DX station's CQ, used to
    /// derive our `tx_parity` (opposite of theirs). May be `None` if
    /// the CQ came from a DX cluster spot rather than an on-air decode.
    ///
    /// This is the autonomous path: the self-duplicate gate applies and
    /// there is no manual keep-calling. For operator-initiated manual
    /// calls use [`Self::respond_to_cq_manual`] (or
    /// [`Self::respond_to_cq_with`] with [`CallInitiation::Manual`]).
    pub async fn respond_to_cq(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Auto,
            None,  // auto path always Tx=Rx; partner_freq not needed
            false, // autonomous is a LOCAL initiation, never remote
        )
        .await
    }

    /// Respond to a CQ call as an operator-initiated **manual** call.
    ///
    /// Bypasses the self-duplicate gate (the operator explicitly chose to
    /// call this station, e.g. to re-work it) and marks the QSO so the
    /// manual keep-calling watchdog re-arms a call every TX slot until the
    /// DX answers or the watchdog fires.
    pub async fn respond_to_cq_manual(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Manual,
            None,  // partner_freq computed by coordinator (T3); None = Tx=Rx fallback
            false, // TUI/DX-hunter manual call is LOCAL, never remote
        )
        .await
    }

    /// Engage a contest profile on an existing QSO. Stamps
    /// `metadata.contest_info` so `process_message_with_parity`'s
    /// reclassification step (PAN-49) knows to try this QSO's engaged
    /// profile's exchange-shape matcher against otherwise-`NonStandard`
    /// decodes, and so ADIF logging picks up `CONTEST_ID` (adif.rs).
    ///
    /// No operator UI calls this yet — a later plan wires the "enter this
    /// contest?" modal to it (docs/superpowers/specs/
    /// 2026-08-30-contest-mode-design.md §4).
    pub async fn engage_contest_profile(
        &self,
        qso_id: QsoId,
        profile: crate::contest::profile::ContestProfile,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        progress.metadata.contest_info = Some(ContestInfo {
            contest_name: profile.id,
            category: String::new(),
            serials: ContestSerials {
                sent: None,
                received: None,
            },
            points: 0,
            multiplier: None,
        });
        Ok(())
    }

    /// Open a manual **Hound** QSO to work a Fox (DXpedition) station.
    ///
    /// Thin wrapper over [`respond_to_cq_with`](Self::respond_to_cq_with) that
    /// additionally sets `metadata.hound = true` and
    /// `metadata.partner_freq = Some(fox_freq)` on the created QSO.
    ///
    /// The Hound procedure (WSJT-X DXpedition mode) splits TX and RX offsets:
    /// - **Our TX offset** (`metadata.frequency`) is placed in the **calling
    ///   region** `[300, 900]` Hz, derived deterministically from `fox_call` so
    ///   concurrent Hound QSOs spread across the pile-up (avoids
    ///   `Math.random`-style non-determinism and keeps tests reproducible).
    /// - **Fox's RX offset** (`metadata.partner_freq`) is `Some(fox_freq)` —
    ///   where we *hear* the Fox. The relevance gate keys on this when set, so
    ///   the Fox's reply on its own offset is correctly routed to this QSO.
    ///
    /// `fox_grid`, if provided, is the Fox's *own* grid square (for
    /// display/logging metadata); it does **not** affect the TX message content
    /// (our opening `<Fox> <us> <grid>` uses *our* grid from station config,
    /// exactly as the normal Caller path does).
    ///
    /// Task 5 (QSY-on-report) mutates `metadata.frequency` upward when the Fox
    /// answers; this constructor only covers the open-and-call-low phase.
    ///
    /// Existing callers of [`respond_to_cq_with`] are unchanged (their
    /// `hound`/`partner_freq` defaults stay `false`/`None`).
    pub async fn engage_hound(
        &self,
        fox_call: &str,
        fox_freq: f64,
        fox_grid: Option<&str>,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        // Compute our deterministic low calling offset for this Fox callsign,
        // using the operator-configured (or default) call region bounds.
        let low_offset = hound_offset_for(
            fox_call,
            self.config.hound.call_min_hz,
            self.config.hound.call_max_hz,
        );

        // Store the Fox's grid on `their_callsign`'s QSO (for logging); the TX
        // message itself uses *our* grid (station config), which respond_to_cq_with
        // already handles via `self.config.our_grid`.
        // fox_grid is accepted for future metadata use (Task 5 / ADIF tagging);
        // we don't thread it into the opening message (same as how the normal path
        // handles the partner's grid — it's not in the CqResponse frame).
        let _ = fox_grid; // used in future tasks (ADIF tag, logging)

        // Open the QSO as a manual Caller at our LOW calling offset. This
        // bypasses the self-duplicate gate (operator explicitly chose this Fox),
        // sets role=Caller, initiated_by=Manual, emits the opening CqResponse
        // (`<Fox> <us> <our_grid>`), and latches tx_parity=dx_parity.opposite().
        // Pass `partner_freq = Some(fox_freq)` directly so the relevance gate is
        // set atomically at construction (no post-insertion mutation needed).
        let qso_id = self
            .respond_to_cq_with(
                fox_call.to_string(),
                low_offset,
                dx_parity,
                CallInitiation::Manual,
                Some(fox_freq), // Fox's RX offset; routes the Fox's reply via partner_freq
                false,          // Shift+H hound engage is a LOCAL operator action
            )
            .await?;

        // Stamp the hound=true flag (partner_freq is now set via the ctor param).
        self.stamp_hound_flag(qso_id).await;

        info!(
            "Hound: engaging Fox {} on partner_freq={:.1} Hz, calling low @ {:.1} Hz: {}",
            fox_call, fox_freq, low_offset, qso_id
        );

        Ok(qso_id)
    }

    /// Atomically stamp `metadata.hound = true` on an already-created QSO and
    /// clear any `pending_freq_drift` candidate that accumulated in the
    /// window between construction (`respond_to_cq_with`, which releases the
    /// write lock after insertion) and this call.
    ///
    /// `metadata.hound` is the sole discriminator `maybe_confirm_frequency_drift_at`
    /// uses to skip genuine Hound/Fox QSOs (Hound has its own one-shot QSY
    /// mechanism and the Fox's RX offset is protocol-fixed for the QSO's
    /// life). Before this QSO is stamped `hound=true`, a decode landing in
    /// that window sees `hound=false` + `partner_freq=Some` and is eligible
    /// to record a drift candidate — it can't relatch alone (two sightings at
    /// least 5 seconds apart are required), but without clearing it here the
    /// candidate is never cleared afterwards either: once `hound=true`, the
    /// skip at the top of `maybe_confirm_frequency_drift_at` `continue`s
    /// before reaching the in-tolerance reset, so a stale `pending_freq_drift`
    /// would persist in serialized metadata for the QSO's life (PAN-15 item 2).
    async fn stamp_hound_flag(&self, qso_id: QsoId) {
        let mut qsos = self.qsos.write().await;
        if let Some(progress) = qsos.get_mut(&qso_id) {
            progress.metadata.hound = true;
            progress.metadata.pending_freq_drift = None;
            // NOTE: do NOT insert "HOUND" into metadata.tags — that would
            // produce a bare `<HOUND:4>true` ADIF field, which is not a
            // valid ADIF name (must be `APP_`-prefixed per the ADIF spec)
            // and can trip LoTW. The human-readable COMMENT "HOUND" and the
            // machine-readable `APP_PANCETTA_HOUND` field are both written
            // by `AdifProcessor::qso_to_adif` from `metadata.hound` directly.
        }
    }

    /// PAN-15 item 3: returns `Some(separation)` when a candidate relatched
    /// `partner_freq` has drifted within [`MIN_TX_SEPARATION_HZ`] of our own
    /// TX offset — the caller should warn, since we may now be keying
    /// directly on top of the station we're trying to hear. Returns `None`
    /// when the separation is still adequate. Pure so the boundary can be
    /// unit-tested without a `QsoManager` instance.
    fn tx_separation_warning(new_partner_freq: f64, our_tx_freq: f64) -> Option<f64> {
        let separation = (new_partner_freq - our_tx_freq).abs();
        (separation < MIN_TX_SEPARATION_HZ).then_some(separation)
    }

    /// Respond to a CQ call, explicitly choosing the initiation mode.
    ///
    /// [`CallInitiation::Auto`] preserves the historical behavior
    /// (duplicate gate enforced, no keep-calling). [`CallInitiation::Manual`]
    /// bypasses the duplicate gate and enables manual keep-calling.
    ///
    /// `partner_freq` — when `Some(f)`, the DX's RX audio offset differs from
    /// our TX offset (`frequency`). `metadata.partner_freq` is set to `f` so the
    /// relevance gate routes the DX's replies (which arrive at *their* audio
    /// offset) to this QSO. Pass `None` for the normal Tx=Rx case (no partner
    /// routing needed). This is the same mechanism `engage_hound` uses.
    pub async fn respond_to_cq_with(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        initiated_by: CallInitiation,
        partner_freq: Option<f64>,
        remote_origin: bool,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        // PAN-23: refuse FT8's literal unresolved-hash placeholder "<...>"
        // outright — shared by `respond_to_cq` (autonomous),
        // `respond_to_cq_manual` (StartQso/DX-Hunter), and `engage_hound`.
        // It carries no identity information and can never be encoded into
        // a transmittable message (see `callsign_is_wire_representable`),
        // so a QSO opened for it is guaranteed to fail later at encode
        // time. Defense in depth: the TUI already filters "<...>" out of
        // every station list an operator can select from (PAN-16/PAN-23);
        // this guards any other path.
        if target_callsign == "<...>" {
            return Err(QsoManagerError::InvalidCallsign {
                callsign: target_callsign,
            });
        }
        // Check for duplicate — but only for autonomous calls. A manual
        // call is an explicit operator decision to work (or re-work) this
        // station, so the self-duplicate gate must not block it.
        if initiated_by == CallInitiation::Auto
            && self.check_duplicate(&target_callsign, frequency).await?
        {
            return Err(QsoManagerError::DuplicateQso {
                callsign: target_callsign,
                frequency,
            });
        }

        // FIX 1: re-calling a station we are ALREADY actively working CONTINUES
        // that QSO instead of superseding it / spawning a duplicate. The
        // on-air failure mode this guards against: an operator mashing Space on
        // one DX previously created a brand-new QSO each press (each
        // superseding the last from the grid step), flooding the single TX
        // worker with stale frames and surfacing the intentional supersede as a
        // scary "QSO … failed: superseded". Now a re-call of an active manual
        // QSO on the same band is an idempotent keep-call: re-send the existing
        // QSO's CURRENT outbound message and return its id. State is untouched
        // (we do NOT reset to RespondingToCq/grid). Only when there is NO active
        // QSO do we fall through to create one (and the genuine
        // re-call-after-terminal case still supersedes any leftover).
        if initiated_by == CallInitiation::Manual {
            if let Some(existing_id) = self
                .find_active_manual_qso_for(&target_callsign, frequency)
                .await
            {
                info!(
                    "Re-call of {} on {:.1} Hz — continuing existing QSO {} (idempotent keep-call, no new QSO)",
                    target_callsign, frequency, existing_id
                );
                // Re-emit the QSO's most-recent outbound as a keep-call. This
                // is a benign no-op if it somehow has no prior Sent message.
                let _ = self.resend_last_tx(existing_id).await;
                return Ok(existing_id);
            }
        }

        // FIX 3: supersede any existing active QSO with this callsign on the
        // same band before creating the new one. With FIX 1 above this should
        // now only ever fire for the genuine case (the older QSO already went
        // terminal but its mapping/record lingered before cleanup). Operator
        // policy: "if there are two exchanges on the same band from the same
        // callsign, use the state of whichever is more recent." We retire the
        // older one (→ Failed{Superseded}, mapping removed) so exactly one QSO
        // per (callsign, band) remains active.
        self.supersede_active_qsos_for(&target_callsign, frequency)
            .await;

        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        // When `dx_parity` is `None` (e.g. answering a DX-cluster/DX-Hunter
        // spot that has never actually been decoded live, so there is no
        // observed slot parity to oppose), fall back to the same "nearest
        // next slot" latch the CQ paths use instead of leaving `tx_parity`
        // permanently `None` (which would make the TX scheduler re-resolve
        // "nearest next slot" independently every subsequent slot and
        // alternate parity — the exact failure `latch_cq_parity_if_none` was
        // built to prevent). Mark the latch `tx_parity_provisional` so the
        // first real decode from this partner can refine it to the true
        // opposite-of-DX parity (see `process_message_for_qso`).
        let (tx_parity, tx_parity_provisional) = match dx_parity {
            Some(p) => (Some(p.opposite()), false),
            None => (Self::latch_cq_parity_if_none(None, now), true),
        };

        let state = QsoState::RespondingToCq {
            target_callsign: target_callsign.clone(),
            frequency,
            started_at: now,
        };

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target_callsign.clone()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by,
            // We answered the DX's CQ → Caller role.
            role: QsoRole::Caller,
            // The first call is emitted immediately below (the CqResponse
            // MessageToSend), so the count starts at 1.
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            // When our TX offset != the DX's RX offset (Hold mode / de-conflict),
            // the caller supplies `partner_freq = Some(dx_freq)` so the relevance
            // gate routes the DX's reply (which arrives at their own audio offset)
            // to this QSO. `None` = Tx=Rx (legacy behavior, unchanged).
            partner_freq,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin,
            tx_parity_provisional,
        };

        // Send response message
        let message = MessageType::CqResponse {
            calling_station: target_callsign.clone(),
            responding_station: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        // Record the initial outbound call as a Sent message so it can be
        // re-sent later (see `resend_last_tx`) and surfaced to the UI. The
        // raw_text is the rendered FT8 text so the TUI "TX:" line shows what
        // we sent (UX audit Batch 2 — was String::new() → blank line).
        let raw_text = self.render_sent_text(&message);
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target_callsign, qso_id).await;

        // Announce the QSO's birth into its initial active state BEFORE the
        // first MessageToSend. The coordinator's `active_tx_qsos` populater
        // inserts a qso_id only on `StateChanged` into an active state; the TX
        // worker's drop-stale-TX gate (Step 4b) then drops any TransmitRequest
        // whose qso_id is absent. Both the StateChanged and the MessageToSend
        // are consumed by the SAME serial event loop in the coordinator, so
        // emitting StateChanged first guarantees the insert is ordered ahead of
        // the TransmitRequest the MessageToSend produces — otherwise the very
        // first scheduled call is silently dropped and PTT never keys (the
        // operator-reported "scheduled QSO never keys PTT, but manual/tune
        // do" bug).
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;

        info!(
            "Responding to CQ from {} on {:.1} Hz: {}",
            target_callsign, frequency, qso_id
        );

        Ok(qso_id)
    }

    /// Respond to a station **calling us**, opening the exchange at an
    /// operator-chosen [`ResponseStep`] instead of always sending our grid.
    ///
    /// This is the engine half of the TUI "Callers" panel. The operator picks
    /// a caller and pancetta classifies what they sent (their CQ/grid →
    /// `Grid`, their report → `ReportAck`, etc.), with a manual override. We
    /// reuse all of [`respond_to_cq_with`](Self::respond_to_cq_with)'s manual
    /// machinery — self-duplicate-gate bypass, superseding a same-call QSO,
    /// latching `tx_parity = dx_parity.opposite()`, and per-slot keep-calling
    /// under the manual watchdog — but set the *initial* [`QsoState`] and emit
    /// the *first* [`MessageType`] according to `step`:
    ///
    /// | step         | initial state          | first message      |
    /// |--------------|------------------------|--------------------|
    /// | `Grid`       | `RespondingToCq`       | `CqResponse` (grid)|
    /// | `Report`     | `SendingReport`        | `SignalReport`     |
    /// | `ReportAck`  | `SendingReport`        | `ReportAck`        |
    /// | `Rr73`       | `WaitingForConfirmation` | `FinalConfirmation` |
    /// | `SeventyThree` | `Completed`          | `SeventyThree` (+ QsoCompleted) |
    ///
    /// `our_snr_of_them` is our measurement of the caller's signal; it
    /// produces the report we send (rounded, clamped to −30..50, defaulting to
    /// −15 if absent). `their_report` is the report they sent us, if known —
    /// used to populate the `their_report` field of the `SendingReport` /
    /// `WaitingForConfirmation` state for `ReportAck`/`Rr73` opens.
    ///
    /// `partner_freq` — when `Some(f)`, our TX offset (`frequency`) differs from
    /// the DX's RX offset `f`. Set by the coordinator when the operator holds a
    /// custom TX offset or when de-confliction nudges us away from the DX's freq.
    /// `None` = Tx=Rx (legacy behavior, unchanged).
    ///
    /// The `Grid` step is exactly equivalent to `respond_to_cq_manual`, so the
    /// DX-Hunter path (which still uses `StartQso` → `respond_to_cq_manual`) is
    /// unaffected.
    // arg count is intrinsic to the QSO-open signature
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_caller(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
        partner_freq: Option<f64>,
        remote_origin: bool,
    ) -> Result<QsoId, QsoManagerError> {
        use pancetta_core::ResponseStep;

        // PAN-23: refuse FT8's literal unresolved-hash placeholder "<...>"
        // up front, before branching on `step`. This is the exact failure
        // mode observed on-air (production logs 2026-08-20..22): an
        // operator selected "<...>" from the TUI Callers panel at step
        // ReportAck, pancetta queued/started the QSO, and the eventual
        // encode attempt failed with "callsign '<...>' cannot be
        // represented in any FT8 message format". The `step == Grid` branch
        // below would also be caught by the identical guard in
        // `respond_to_cq_with`, but checking here first (a) covers every
        // other step (Report/ReportAck/Rr73/SeventyThree) that bypasses
        // `respond_to_cq_with` entirely, and (b) fails fast before any
        // logging/queueing work happens. Defense in depth: the TUI already
        // filters "<...>" out of the Callers list an operator can select
        // from (PAN-16/PAN-23); this guards any other path — present or
        // future — that might route a `RespondToCaller` command here.
        if target == "<...>" {
            return Err(QsoManagerError::InvalidCallsign { callsign: target });
        }

        // Grid is exactly the historical manual-call behavior; route through
        // the existing path so there is a single source of truth for it.
        if step == ResponseStep::Grid {
            return self
                .respond_to_cq_with(
                    target,
                    frequency,
                    dx_parity,
                    CallInitiation::Manual,
                    partner_freq,
                    remote_origin,
                )
                .await;
        }

        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }

        // Our report of their signal (the report WE send), same formula the
        // auto-sequencer uses in `MessageExchange::generate_response`, but
        // defaulting to -15 when we have no measurement.
        let our_report: SignalReport = our_snr_of_them
            .map(|s| (s.round() as i8).clamp(-30, 50))
            .unwrap_or(-15);
        // Their report of us (only meaningful for ReportAck/Rr73 opens).
        let their_report_val: SignalReport = their_report.unwrap_or(-15);

        // FIX 1: if we already have an ACTIVE manual QSO with this caller on
        // this band, CONTINUE it instead of superseding/duplicating. Mashing a
        // context reply on a station already in progress must keep ONE QSO per
        // (callsign, band).
        //   - If the requested step is AHEAD of the existing QSO's current
        //     ladder stage (e.g. the DX now sent RR73 → SeventyThree while we
        //     were in SendingReport), advance the EXISTING QSO to emit that
        //     step.
        //   - If it matches (or is behind) the current stage, just re-send the
        //     existing QSO's current outbound (idempotent keep-call).
        // Either way we return the existing id and never create a second QSO.
        if let Some(existing_id) = self.find_active_manual_qso_for(&target, frequency).await {
            let existing_rank = {
                let qsos = self.qsos.read().await;
                qsos.get(&existing_id)
                    .and_then(|p| Self::ladder_rank(&p.state))
            };
            let requested_rank = Self::step_ladder_rank(step);
            match (existing_rank, requested_rank) {
                (Some(cur), Some(req)) if req > cur => {
                    info!(
                        "Context reply to {} at step {:?} — advancing existing QSO {} \
                         (ahead of its current stage)",
                        target, step, existing_id
                    );
                    self.advance_existing_qso_to_step(
                        existing_id,
                        &target,
                        frequency,
                        step,
                        our_report,
                        their_report_val,
                    )
                    .await?;
                    return Ok(existing_id);
                }
                _ => {
                    info!(
                        "Context reply to {} at step {:?} — re-sending existing QSO {} \
                         current outbound (idempotent keep-call)",
                        target, step, existing_id
                    );
                    let _ = self.resend_last_tx(existing_id).await;
                    return Ok(existing_id);
                }
            }
        }

        // FIX B (one-QSO-per-(callsign,band) close idempotency,
        // docs/qso-tx-deep-review-2026-07-18.md): no ACTIVE QSO with this
        // caller (FIX 1 above didn't fire) — but if the requested step is a
        // CLOSE step (Rr73/SeventyThree) and we JUST completed a manual QSO
        // with this exact station on this band, within the grace window, this
        // is the operator mashing "send RR73"/"send 73" again after the QSO
        // already finished (observed on-air: 3 SeventyThree presses in 8s
        // produced 4 duplicate ADIF log entries for the same contact). Re-key
        // the EXISTING completed QSO's last frame instead of opening a new
        // one — never spawn a sibling `QsoId`, never emit a second
        // `QsoCompleted`. Grid/Report/ReportAck (ladder rank < 2) fall
        // through to the normal new-QSO path below: a fresh call at those
        // steps within the grace window is the deliberate legitimate-rework
        // case (e.g. working the same station again on a different exchange).
        if Self::step_ladder_rank(step) >= Some(2) {
            if let Some(existing_id) = self
                .find_recently_completed_manual_qso_for(
                    &target,
                    frequency,
                    COMPLETED_QSO_REWORK_GRACE,
                )
                .await
            {
                info!(
                    "Context reply to {} at step {:?} — DX already worked within the \
                     grace window, re-sending existing QSO {}'s last frame",
                    target, step, existing_id
                );
                let _ = self.resend_last_tx(existing_id).await;
                return Ok(existing_id);
            }
        }

        // Manual: supersede any same-call QSO on this band, then build the new
        // one (no duplicate gate — the operator explicitly chose this caller).
        // With FIX 1 above this only fires when no ACTIVE QSO remains (e.g. a
        // lingering terminal record), so it should rarely trigger now.
        self.supersede_active_qsos_for(&target, frequency).await;

        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        // See the matching comment in `respond_to_cq_with`: a `None`
        // `dx_parity` (no observed decode for this caller yet) falls back to
        // the "nearest next slot" latch and is marked provisional so the
        // first real decode from this partner can refine it.
        let (tx_parity, tx_parity_provisional) = match dx_parity {
            Some(p) => (Some(p.opposite()), false),
            None => (Self::latch_cq_parity_if_none(None, now), true),
        };

        // Build (initial_state, first_message) for the chosen step.
        let (state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => unreachable!("Grid handled above"),
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target.clone()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by: CallInitiation::Manual,
            // Replying to a station calling us → Caller role.
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            // When our TX offset != the DX's RX offset (Hold mode / de-conflict),
            // the caller supplies `partner_freq = Some(dx_freq)` so the relevance
            // gate routes the DX's reply (which arrives at their audio offset) to
            // this QSO. `None` = Tx=Rx (legacy behavior, unchanged).
            partner_freq,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin,
            tx_parity_provisional,
        };

        // If we are opening at the close (SeventyThree → Completed), stamp the
        // completion reports/end-time so the logged record is well-formed.
        let is_completed_open = matches!(state, QsoState::Completed { .. });
        if is_completed_open {
            metadata.reports = SignalReports {
                sent: Some(our_report),
                received: Some(their_report_val),
            };
            metadata.end_time = Some(now);
        }

        let raw_text = self.render_sent_text(&message);
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata: metadata.clone(),
        };
        // Captured before the move into the map below so the
        // QsoCompleted-on-open-at-close emit further down still has it
        // (Layer 2 timeline persistence).
        let opening_state_history = progress.state_history.clone();
        let opening_messages = progress.messages.clone();

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target, qso_id).await;

        // See the matching comment in `respond_to_cq_with`: emit the initial
        // StateChanged BEFORE the first MessageToSend so the coordinator's
        // `active_tx_qsos` set has this qso_id inserted before the first
        // scheduled TransmitRequest reaches the Step 4b PTT gate. A
        // `Completed` open (SeventyThree) is not an active state, but emitting
        // the StateChanged is still correct/harmless — the gate's post-
        // completion grace window keeps the final-73 TransmitRequest live (and
        // the QsoCompleted event below drives the grace insert+removal anyway).
        self.emit_state_change(qso_id, QsoState::Idle, state.clone())
            .await;

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;

        info!(
            "Responding to caller {} on {:.1} Hz at step {:?}: {}",
            target, frequency, step, qso_id
        );

        // If we opened directly at the close, emit QsoCompleted so the logger
        // records the QSO. Mirror the completion metadata path in
        // `process_message_for_qso`, including the dial-frequency stamp so the
        // ADIF carries the real on-air RF frequency, not the bare audio offset.
        if is_completed_open {
            let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
            let dial = effective_tx_dial(rx_dial, split);
            if dial > 0 {
                let rf_frequency = metadata.frequency + dial as f64;
                // Only the LOCAL `metadata` (used for the emitted event) gets
                // `frequency` overwritten with the RF value -- the STORED
                // entry's `frequency` must stay the audio offset (PAN-25
                // round 2: see `QsoMetadata::frequency`'s doc comment for
                // why). Instead, stamp the stored entry's
                // `completed_rf_frequency_hz` for the recent-completed
                // suppression's band check, same as the
                // `process_message_for_qso` completion path.
                metadata.frequency = rf_frequency;
                if let Some(stored) = self.qsos.write().await.get_mut(&qso_id) {
                    stored.metadata.completed_rf_frequency_hz = Some(rf_frequency);
                }
            }
            self.emit_event(QsoEvent::QsoCompleted {
                qso_id,
                metadata,
                state_history: opening_state_history,
                messages: opening_messages,
            })
            .await;
        }

        Ok(qso_id)
    }

    /// Two-strike confirm-before-relatch at the current time. See
    /// [`Self::maybe_confirm_frequency_drift_at`].
    async fn maybe_confirm_frequency_drift(&self, message_type: &MessageType, frequency: f64) {
        self.maybe_confirm_frequency_drift_at(message_type, frequency, Utc::now())
            .await;
    }

    /// Two-strike confirm-before-relatch at an explicit time (for testability,
    /// mirroring [`Self::check_timeouts_at`]'s pattern): track a pending off-latch
    /// frequency candidate per QSO, and only relatch once a SECOND message from the
    /// same identified partner repeats at the same new frequency AT LEAST 5 real
    /// seconds after the candidate was FIRST noted. Ordinary Tx=Rx QSOs relatch both
    /// `metadata.frequency` and the active state's embedded `frequency`; non-Hound
    /// split-TX QSOs relatch only `metadata.partner_freq`, preserving our deliberate
    /// TX offset. Runs BEFORE the existing
    /// `find_qsos_for_message`/`is_message_relevant` routing, which stays completely
    /// unmodified — see
    /// `docs/superpowers/specs/2026-07-27-qso-frequency-relatch-v2-design.md`.
    ///
    /// A single off-frequency identity-matching message can't be safely distinguished
    /// from a spoofed frame claiming the partner's callsign (FT8 decoded text carries
    /// no cryptographic identity) — see
    /// `adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`. Only
    /// a REPEATED match at the same new frequency is trusted — and "repeated" must mean
    /// a genuinely separate transmission, not a second decode-pipeline copy of the same
    /// one. The hb-091 scoped fast-path decodes the same audio window twice and forwards
    /// both copies here (this pre-pass runs before the dedup that
    /// `is_message_relevant` used to provide), landing within milliseconds of each
    /// other, so a bare "matches an existing candidate" check is satisfiable by ONE
    /// physical transmission. The 5s floor is comfortably below FT4's 7.5s slot period
    /// (the shortest slot this app supports), so two genuinely separate transmissions
    /// always clear it while two decode-pipeline copies of one transmission never do.
    ///
    /// A same-frequency sighting that arrives again WITHIN the 5s gap does NOT reset
    /// the pending candidate's timestamp — it is presumed a duplicate delivery of the
    /// same transmission, and the ORIGINAL timestamp is preserved untouched. If the
    /// timestamp were reset on every near-duplicate, repeated fast deliveries (spoofed
    /// or an artifact of the decode pipeline) could push confirmation out indefinitely;
    /// a real DX repeating naturally will eventually land >=5s after the ORIGINAL
    /// sighting and correctly confirm.
    async fn maybe_confirm_frequency_drift_at(
        &self,
        message_type: &MessageType,
        frequency: f64,
        now: DateTime<Utc>,
    ) {
        // Reuses the module-level `ESTABLISHED_FREQ_TOLERANCE_HZ`/
        // `FREQ_TOLERANCE_HZ` shared with `is_message_relevant`/
        // `classify_relevance` (PAN-15 item 6 hoisted all three sites onto
        // one definition — previously each redeclared its own local copy of
        // the same values). `DRIFT_CONFIRM_TOLERANCE_HZ` keeps its own name
        // here since it means something distinct ("counts as the same spot"
        // for two-strike confirmation) even though the value matches
        // `FREQ_TOLERANCE_HZ`.
        const DRIFT_CONFIRM_TOLERANCE_HZ: f64 = FREQ_TOLERANCE_HZ;
        // Confirming sighting must land >=5 real seconds after the ORIGINAL candidate
        // timestamp — comfortably below FT4's 7.5s slot period, so two genuinely
        // separate transmissions always clear it while two decode-pipeline copies of
        // the SAME transmission (milliseconds apart) never do.
        const DRIFT_CONFIRM_MIN_GAP: chrono::Duration = chrono::Duration::seconds(5);
        // Wide RX-plausibility sanity bound for candidate ELIGIBILITY -- deliberately
        // NOT the same as TX_OFFSET_MIN_HZ/MAX_HZ (which govern where we autonomously
        // PICK a fresh TX offset, not where we're willing to consider a real decoded
        // signal a real DX). Responding to a CQ already ties our reply to wherever we
        // decoded it, unclamped, for any DX up to ~2900 Hz -- this must preserve that,
        // not narrow it. The constraint that actually matters here is the modulator's
        // transmittable envelope (`pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` =
        // 3100.0, verified to cover a 2900 Hz base plus the widest FT2 tone spread) --
        // NOT `frequency.rs`/`autonomous.rs`'s own convention, which is actually
        // 200-2800 (narrower on the top end) and governs a different concern (where
        // the autonomous allocator prefers to place a fresh CQ/answer).
        const DRIFT_CANDIDATE_MIN_HZ: f64 = 200.0;
        const DRIFT_CANDIDATE_MAX_HZ: f64 = 2900.0;

        let Some(sender) = message_type.sender_callsign() else {
            return;
        };
        if !message_type.is_addressed_to(&self.config.our_callsign) {
            return;
        }

        let mut qsos = self.qsos.write().await;
        for (&qso_id, progress) in qsos.iter_mut() {
            if !progress.state.is_active() {
                continue;
            }
            if progress.metadata.hound {
                // Genuine Hound/Fox only. The Fox's RX offset is protocol-fixed for
                // the QSO's life and Hound has its own one-shot QSY mechanism.
                // `partner_freq` alone is not a Hound discriminator: ordinary manual
                // QSOs also set it after an offset hold, collision nudge, or clamp.
                continue;
            }
            let Some(their_callsign) = progress.state.their_callsign().map(|s| s.to_string())
            else {
                continue; // Pre-establishment (CallingCq/Idle) — not this mechanism's scope.
            };
            if !Self::is_partner(sender, &their_callsign) {
                continue;
            }
            let Some(qso_freq) = progress.state.frequency() else {
                continue;
            };
            // Where we expect to hear the DX. This is deliberately identical to
            // is_message_relevant's baseline so both gates agree on their location.
            let split_tx = progress.metadata.partner_freq;
            let match_freq = split_tx.unwrap_or(qso_freq);
            let distance = (match_freq - frequency).abs();

            if distance <= ESTABLISHED_FREQ_TOLERANCE_HZ {
                progress.metadata.pending_freq_drift = None;
                continue;
            }

            if !(DRIFT_CANDIDATE_MIN_HZ..=DRIFT_CANDIDATE_MAX_HZ).contains(&frequency) {
                continue; // Out-of-band decode — don't let noise reset a real candidate.
            }

            let confirmed = progress.metadata.pending_freq_drift.is_some_and(|(f, t)| {
                (f - frequency).abs() <= DRIFT_CONFIRM_TOLERANCE_HZ
                    && now - t >= DRIFT_CONFIRM_MIN_GAP
            });

            if confirmed {
                progress.metadata.pending_freq_drift = None;
                if split_tx.is_some() {
                    progress.metadata.partner_freq = Some(frequency);
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_partner_freq = match_freq,
                        new_partner_freq = frequency,
                        our_tx_freq = qso_freq,
                        "confirmed partner drift on a split-TX QSO (2 consistent sightings, \
                         >=5s apart) — relatching partner_freq; our TX offset is unchanged"
                    );
                    // PAN-15 item 3: the relatched partner_freq is bounded only
                    // by the RX-plausibility window above — it's never checked
                    // against our own TX offset (`metadata.frequency`) or other
                    // active TX offsets. A DX that drifts onto (or within
                    // MIN_TX_SEPARATION_HZ of) our latched TX offset would have
                    // us keying directly on top of the station we're trying to
                    // hear, silently defeating whatever collision nudge/clamp
                    // separated them originally. No re-deconfliction is
                    // performed (stretch goal); this is the minimum-bar warn.
                    if let Some(tx_separation) =
                        Self::tx_separation_warning(frequency, progress.metadata.frequency)
                    {
                        warn!(
                            target: "qso.freq_gate",
                            partner = %their_callsign,
                            new_partner_freq = frequency,
                            our_tx_freq = progress.metadata.frequency,
                            separation_hz = tx_separation,
                            "relatched partner_freq drifted within MIN_TX_SEPARATION_HZ of our \
                             own TX offset — we may now be keying on top of the station we're \
                             trying to hear; no re-deconfliction performed"
                        );
                    }
                    // The QSO state itself is deliberately untouched here (only
                    // where we HEAR the DX moved; our TX offset did not), so
                    // old_state == new_state. We still emit, because the
                    // coordinator refreshes its scoped/AP decoder hint
                    // (`active_qso_freq_hz`) only in its `StateChanged` handler,
                    // re-reading `metadata.partner_freq` there. Without this the
                    // decoder stays centred on the obsolete partner offset while
                    // the relevance gate has already moved to the new one —
                    // a confirmation arriving on a frame that leaves the state
                    // unchanged (e.g. a repeated `SignalReport` in
                    // `SendingReport`) has no other transition to piggyback on,
                    // so weak replies at the relatched frequency would lose
                    // narrow-band/AP recovery until some unrelated transition.
                    // The handler is idempotent for a same-state event: it
                    // re-inserts into `active_tx_qsos`/`active_tx_offsets` (both
                    // keyed on the unchanged TX offset), recomputes AP context,
                    // and re-pushes the TUI snapshot. It triggers no TX.
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                } else {
                    let old_state = progress.state.clone();
                    progress.metadata.frequency = frequency;
                    match &mut progress.state {
                        QsoState::RespondingToCq {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::Contest(ContestState::ExchangingInfo {
                            frequency: state_freq,
                            ..
                        })
                        | QsoState::Contest(ContestState::ContestCompleted {
                            frequency: state_freq,
                            ..
                        }) => {
                            *state_freq = frequency;
                        }
                        _ => {
                            debug_assert!(
                                false,
                                "confirmed drift relatch has no frequency-field mutation arm for \
                                 this QsoState variant — metadata.frequency and the state's own \
                                 frequency field are now desynced"
                            );
                        }
                    }
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_freq = qso_freq,
                        new_freq = frequency,
                        "confirmed frequency drift (2 consistent sightings, >=5s apart) — \
                         relatching QSO"
                    );
                    self.emit_state_change(qso_id, old_state, progress.state.clone())
                        .await;
                }
            } else {
                // Only start (or leave untouched) a pending candidate — see the
                // "duplicate delivery" doc comment above for why an existing
                // candidate's timestamp is never bumped forward here.
                if progress
                    .metadata
                    .pending_freq_drift
                    .is_none_or(|(f, _)| (f - frequency).abs() > DRIFT_CONFIRM_TOLERANCE_HZ)
                {
                    progress.metadata.pending_freq_drift = Some((frequency, now));
                    debug!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        candidate_freq = frequency,
                        latched_freq = match_freq,
                        "identity-verified message outside tolerance — noting drift candidate \
                         (needs 1 more confirming sighting >=5s later)"
                    );
                }
            }
        }
    }

    /// Process an incoming message.
    ///
    /// Does not carry a decoded slot parity — the first-decode provisional-
    /// parity refinement (see `process_message_for_qso`) is a no-op on this
    /// path. Callers that have a real observed slot parity for the decode
    /// (the live coordinator decode path) should use
    /// [`Self::process_message_with_parity`] instead so a provisionally-
    /// latched QSO parity can be refined.
    pub async fn process_message(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
    ) -> Result<(), QsoManagerError> {
        self.process_message_with_parity(message_type, raw_text, frequency, signal_strength, None)
            .await
    }

    /// Process an incoming message that carries a known decoded slot parity.
    ///
    /// Identical to [`Self::process_message`] except `observed_slot_parity` is
    /// threaded through to `process_message_for_qso`, where it drives the
    /// first-decode provisional-parity refinement: a QSO whose `tx_parity`
    /// was latched without ever having observed the DX's real slot parity
    /// (see `QsoMetadata::tx_parity_provisional`) gets that latch corrected
    /// to the true opposite-of-DX parity the first time a verified frame from
    /// the latched partner arrives. Pass `None` when the slot parity of this
    /// decode is not tracked (byte-identical to `process_message`).
    pub async fn process_message_with_parity(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
        observed_slot_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<(), QsoManagerError> {
        let timestamp = Utc::now();

        // PAN-49: an otherwise-unclassifiable decode may be a contest ack
        // (e.g. "R"+grid) that `MessageExchange::parse_message` has no
        // pattern for. Only reinterpret it when the incoming frame's sender
        // matches the PARTNER of a QSO that is itself contest-engaged
        // (`engage_contest_profile`) — scoped per-QSO, not "any QSO
        // anywhere is engaged" (final review Finding 2: that global gate let
        // a frame from an unrelated third party get reclassified and routed
        // to someone else's engaged QSO). This keeps every QSO whose partner
        // did not send this frame — including every non-contest QSO —
        // byte-identical to today; Finding 1's sender-gated grid latch is
        // the remaining defense-in-depth layer once a frame does reach the
        // transition arm.
        let message_type = match &message_type {
            MessageType::NonStandard { .. } => {
                match crate::contest::matcher::match_grid_with_r_ack(&raw_text) {
                    Some(m) => {
                        let any_engaged = {
                            let qsos = self.qsos.read().await;
                            qsos.values().any(|p| {
                                p.state.is_active()
                                    && p.metadata.contest_info.is_some()
                                    && p.state.their_callsign().is_some_and(|c| {
                                        crate::exchange::callsigns_match(c, &m.from_station)
                                    })
                            })
                        };
                        if any_engaged {
                            MessageType::ContestReply {
                                to_station: m.to_station,
                                from_station: m.from_station,
                                grid: m.grid,
                                is_ack: true,
                            }
                        } else {
                            message_type
                        }
                    }
                    None => message_type,
                }
            }
            _ => message_type,
        };

        self.maybe_confirm_frequency_drift(&message_type, frequency)
            .await;

        // Find relevant QSO(s)
        let qso_ids = self.find_qsos_for_message(&message_type, frequency).await;

        for qso_id in qso_ids {
            let message = QsoMessage {
                timestamp,
                direction: MessageDirection::Received,
                message_type: message_type.clone(),
                raw_text: raw_text.clone(),
                signal_strength,
                frequency,
            };

            self.process_message_for_qso(qso_id, message, observed_slot_parity)
                .await?;
        }

        Ok(())
    }

    /// Get QSO status
    pub async fn get_qso(&self, qso_id: QsoId) -> Result<QsoProgress, QsoManagerError> {
        let qsos = self.qsos.read().await;
        qsos.get(&qso_id)
            .cloned()
            .ok_or(QsoManagerError::QsoNotFound { qso_id })
    }

    /// Get all active QSOs
    pub async fn get_active_qsos(&self) -> Vec<(QsoId, QsoProgress)> {
        let qsos = self.qsos.read().await;
        qsos.iter()
            .filter(|(_, progress)| progress.state.is_active())
            .map(|(id, progress)| (*id, progress.clone()))
            .collect()
    }

    /// Return the TX audio offsets (Hz) of all currently-active (non-terminal)
    /// QSOs. Used by the coordinator at QSO-open time to de-conflict a new QSO's
    /// TX offset against live concurrent streams so no two QSOs transmit on the
    /// same (or near) audio frequency.
    pub async fn active_tx_offsets(&self) -> Vec<f64> {
        let qsos = self.qsos.read().await;
        qsos.values()
            .filter(|p| p.state.is_active())
            .map(|p| p.metadata.frequency)
            .collect()
    }

    /// Cancel a QSO
    pub async fn cancel_qso(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        if let Some(mut progress) = qsos.remove(&qso_id) {
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: QsoFailureReason::UserCancelled,
                failed_at: Utc::now(),
                last_state: Box::new(old_state.clone()),
            };
            // Capture metadata before it's consumed below (Batch 4, SM-F5).
            let metadata = progress.metadata.clone();
            // Layer 2 timeline persistence: capture the full timeline before
            // `progress` is dropped at the end of this block — this is the
            // "QSO leaves the active map" discard site.
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();

            self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                .await;
            // SM-F5: this producer previously emitted only StateChanged →
            // Failed, leaving `QsoEvent::QsoFailed` dead — the coordinator's
            // priority-scoring failure backoff (`record_failure`) subscribes
            // to QsoFailed specifically, so a cancelled QSO never counted
            // against a station's priority score. The coordinator's
            // active_tx_qsos removal is a HashSet::remove on both this event
            // and the StateChanged above, so emitting both is idempotent.
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason: QsoFailureReason::UserCancelled,
                last_state: old_state,
                metadata,
                state_history,
                messages,
            })
            .await;

            // Remove from callsign mapping
            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }

            info!("Cancelled QSO: {}", qso_id);
        }

        Ok(())
    }

    /// Fail a QSO with an explicit `reason`, transitioning it to
    /// `QsoState::Failed` and emitting both `QsoEvent::StateChanged` and
    /// `QsoEvent::QsoFailed` — the same producer pattern `cancel_qso` (fixed
    /// at `UserCancelled`) and the supersede/timeout retirement paths each
    /// use inline, generalized here to an arbitrary reason so callers
    /// outside this module (the coordinator's task supervisor, specifically)
    /// can retire a QSO through the manager's real state machine instead of
    /// constructing a `QsoEvent` by hand. Currently used for
    /// `QsoFailureReason::SupervisorRestart`: when the Qso component's task
    /// is restarted after a panic, every QSO still active at that moment is
    /// surfaced this way rather than silently vanishing from the map.
    pub async fn fail_qso(
        &self,
        qso_id: QsoId,
        reason: QsoFailureReason,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        if let Some(mut progress) = qsos.remove(&qso_id) {
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: reason.clone(),
                failed_at: Utc::now(),
                last_state: Box::new(old_state.clone()),
            };
            let metadata = progress.metadata.clone();
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();

            self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                .await;
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason,
                last_state: old_state,
                metadata,
                state_history,
                messages,
            })
            .await;

            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }

            info!("Failed QSO {}: {:?}", qso_id, progress.state);
        }

        Ok(())
    }

    /// Emit a MessageToSend event for a QSO.
    ///
    /// Reads `tx_parity` from the QSO metadata so that every emission
    /// carries the value latched at QSO start, regardless of when this
    /// method is called.  Used by the auto_sequencer internally and
    /// exposed as `pub` so integration tests can drive additional
    /// MessageToSend events without going through the auto_sequencer.
    pub async fn send_message(&self, qso_id: QsoId, message: MessageType, frequency: f64) {
        let (tx_parity, remote_origin) = self
            .qsos
            .read()
            .await
            .get(&qso_id)
            .map(|p| (p.metadata.tx_parity, p.metadata.remote_origin))
            .unwrap_or((None, false));
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;
    }

    /// Re-send the most recent outbound message for a QSO.
    ///
    /// Looks up the QSO, finds the most-recent `Sent` message in its message
    /// log, and re-emits it via the same `MessageToSend` path `send_message`
    /// uses (carrying the QSO's frequency and latched `tx_parity`). Returns
    /// `QsoNotFound` for an unknown id; returns `Ok(())` (a benign no-op) when
    /// the QSO has no prior outbound message to resend.
    ///
    /// Stamps `last_call_at` (and bumps `call_count`) exactly like the
    /// keep-call rearm (`rearm_manual_calls_at`) does on every re-emission,
    /// so an operator-triggered resend is accounted the same way a
    /// rearm-driven send is for watchdog/cap purposes, and so an operator
    /// re-pressing Space/resend near a slot boundary can't produce a
    /// same-text emission the rearm/coalescer machinery doesn't know about.
    pub async fn resend_last_tx(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let now = Utc::now();
        let (message, frequency) = {
            let mut qsos = self.qsos.write().await;
            let progress = qsos
                .get_mut(&qso_id)
                .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
            match progress
                .messages
                .iter()
                .rev()
                .find(|m| m.direction == MessageDirection::Sent)
            {
                Some(m) => {
                    let message = m.message_type.clone();
                    let frequency = progress.metadata.frequency;
                    progress.metadata.call_count += 1;
                    progress.metadata.last_call_at = Some(now);
                    (message, frequency)
                }
                None => {
                    info!("resend_last_tx: no prior Sent message for QSO {}", qso_id);
                    return Ok(());
                }
            }
        };
        self.send_message(qso_id, message, frequency).await;
        Ok(())
    }

    /// External commit point for PAN-72's stall-switch/revert and the manual
    /// nudge keystroke — the only way outside code may change an
    /// already-active QSO's TX offset. Mirrors the mutation the removed
    /// stuck-DX hop used to perform inline; the caller (the coordinator) has
    /// already resolved WHAT the new offset should be (via the smart
    /// allocator for a Switch, or `last_known_good_offset_hz` for a Revert —
    /// `QsoManager` itself never calls the allocator, see the single-scorer
    /// invariant). Does not force an immediate retransmission — the next
    /// naturally-scheduled send picks up the new value since message
    /// construction reads `metadata.frequency` fresh each time.
    ///
    /// The requested offset is clamped defensively to
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]..=[`ACTIVE_QSO_TX_OFFSET_MAX_HZ`] —
    /// this is a `pub` method documented as the sole external mutator of an
    /// active QSO's TX offset, so it must not be able to park a QSO outside
    /// the transmittable envelope from outside the crate. Every current
    /// caller already supplies an in-band value.
    ///
    /// NOT [`TX_OFFSET_MIN_HZ`]..=[`TX_OFFSET_MAX_HZ`] (300–2700), the band
    /// the removed stuck-DX hop used: those govern where we autonomously
    /// *pick a fresh* offset, not where an in-progress QSO is allowed to sit.
    /// A `Revert`'s `target_hz` is a `last_known_good_offset_hz` that may be
    /// an unclamped reply offset up to ~2900 Hz (answering a CQ ties our reply
    /// to wherever we decoded the DX), and narrowing that to 2700 would move
    /// the QSO off the very offset it demonstrably worked on. See
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]'s doc comment.
    ///
    /// Returns the offset that was **actually applied** (post-clamp), so the
    /// coordinator can mirror it into its own `active_tx_offsets` snapshot
    /// without re-deriving the clamp.
    ///
    /// Four guards run BEFORE anything is mutated. The first three are
    /// staleness guards, all because the coordinator only drains its request
    /// mailbox once per 15-second slot, so a lot can happen between an action
    /// being raised and committed; the fourth rejects a move that isn't one:
    ///
    /// 1. **Terminal QSO** (Codex round 1 on PR #350, finding 3). Completed
    ///    entries are retained in the map, and completion deliberately keeps
    ///    the QSO's id and offset in the coordinator's active snapshots for a
    ///    45-second trailing-73 grace window — so a late action still *finds*
    ///    its QSO. `QsoState::set_frequency` already refuses terminal states,
    ///    but `metadata.frequency` has no such guard: writing it would leave
    ///    the metadata disagreeing with the logged state, and the drain would
    ///    mirror that phantom offset into `active_tx_offsets` while the queued
    ///    final frame still goes out on the original one. Refuse outright.
    /// 2. **Superseded by a DX advance** (finding 8). If the QSO's
    ///    [`crate::states::QsoMetadata::advance_generation`] has moved since
    ///    `raised_at_generation` was captured, the DX answered in the
    ///    meantime: `stall_cycles` is already reset and the CURRENT offset is
    ///    already recorded known-good, so applying the queued action would
    ///    move us off the offset that just demonstrably worked. Pass `None`
    ///    for an operator-forced nudge, which is current by construction and
    ///    must never be discarded this way.
    /// 3. **Outside the live Hound region** (Codex round 4, finding 2). A
    ///    Hound's TX offset is procedurally pinned to the calling region before
    ///    the Fox answers and to the response region after the QSY, and the
    ///    coordinator picks that window from a `get_qso` snapshot taken before
    ///    the allocator runs. The QSY can land in between — and for an
    ///    operator-forced `u` nudge, which carries no generation, guard 2
    ///    cannot catch it — so the window is re-derived here from the locked
    ///    `progress` (through the same [`hound_switch_range_hz`] the
    ///    coordinator resolved with) and an out-of-region offset is refused
    ///    rather than committed. Refusing, not re-clamping: the stall evidence
    ///    survives and the detector re-raises against the correct region on its
    ///    own, whereas a clamp would park us on a region edge nothing ranked.
    /// 4. **No-op relocation** (PAN-79; Codex round 3, finding 2). The
    ///    allocator's "no valid relocation exists" signal is `avoid_hz`
    ///    returned unchanged, which reaches this method as a requested offset
    ///    equal (within [`TX_OFFSET_NOOP_TOLERANCE_HZ`]) to the QSO's current
    ///    one. Nothing relocates, so nothing may be committed: clearing
    ///    `stall_cycles` here would throw away valid accumulated stall evidence
    ///    — the operator's `u` nudge over a partial streak is the live case —
    ///    and push automatic recovery out by another full
    ///    `qso_stall_switch_after` window, while `TxOffsetApplied` would
    ///    announce a move that never happened.
    ///
    /// All four refusals are expected outcomes, not faults — see
    /// [`QsoManagerError::is_expected_offset_action_refusal`].
    pub async fn apply_tx_offset_switch(
        &self,
        qso_id: QsoId,
        new_offset_hz: f64,
        raised_at_generation: Option<u32>,
    ) -> Result<f64, QsoManagerError> {
        let applied_hz =
            new_offset_hz.clamp(ACTIVE_QSO_TX_OFFSET_MIN_HZ, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        if !progress.state.is_active() {
            return Err(QsoManagerError::QsoNotActive { qso_id });
        }
        if let Some(raised_at) = raised_at_generation {
            let current = progress.metadata.advance_generation;
            if current != raised_at {
                return Err(QsoManagerError::OffsetActionStale {
                    qso_id,
                    raised_at,
                    current,
                });
            }
        }
        // 3. **Outside the live Hound region** (Codex round 4 on PR #350,
        //    finding 2). The coordinator picks the window from a `get_qso`
        //    snapshot taken BEFORE the allocator runs, and the Fox's first
        //    report performs the mandatory calling-region -> response-region
        //    QSY (`process_message_for_qso`'s Hound block, which flips
        //    `hound_qsyed`). An operator-forced `u` nudge deliberately carries
        //    no `raised_at_generation`, so guard 2 above cannot catch that
        //    advance — the low, calling-region offset would commit and undo the
        //    QSY, putting our R+report where the Fox is not listening.
        //    Re-deriving the window HERE, from the same locked `progress` this
        //    method is about to mutate and through the same
        //    `hound_switch_range_hz` the coordinator resolved with, makes the
        //    check and the commit atomic by construction.
        if let Some((min_hz, max_hz)) = hound_switch_range_for(&progress.metadata, &self.config) {
            if applied_hz < min_hz || applied_hz > max_hz {
                return Err(QsoManagerError::OffsetActionOutsideHoundRegion {
                    qso_id,
                    offset_hz: applied_hz,
                    min_hz,
                    max_hz,
                });
            }
        }
        let old_off = progress.metadata.frequency;
        // 4. **Nothing to relocate** (PAN-79; Codex round 3 on PR #350, finding
        //    2). The allocator returns `avoid_hz` unchanged when every
        //    candidate is excluded, and the drain passes that straight here, so
        //    a "switch" can resolve back onto the offset the QSO is already on.
        //    Committing it would clear `stall_cycles` — valid evidence, and on
        //    an operator `u` nudge over a partial streak the ONLY evidence, so
        //    automatic recovery would be pushed out by another full
        //    `qso_stall_switch_after` window — and emit a `TxOffsetApplied`
        //    announcing a move that never happened. Refuse before mutating.
        if (applied_hz - old_off).abs() <= TX_OFFSET_NOOP_TOLERANCE_HZ {
            return Err(QsoManagerError::OffsetActionNoOp {
                qso_id,
                offset_hz: old_off,
            });
        }
        progress.metadata.frequency = applied_hz;
        progress.metadata.pending_freq_drift = None;
        progress.metadata.stall_cycles = 0;
        // PAN-72 (Codex round 4 on PR #350, finding 3): remember the offset we
        // are vacating, and when. The frame that triggered this move went out
        // on it — `rearm_manual_calls_at` re-sends and trips the stall
        // threshold in the same pass, and this commit only happens on the
        // coordinator's next drain — so any answer to that frame arrives at
        // `old_off` AFTER this mutation. For `PRE_SWITCH_OFFSET_GRACE` the
        // vacated offset therefore stays a valid RX baseline
        // (`is_message_relevant`) and stays the offset a forward advance
        // credits as known-good (`process_message_for_qso`). See
        // `QsoMetadata::pre_switch_offset`.
        progress.metadata.pre_switch_offset = Some((old_off, Utc::now()));
        // Mirror the Hound QSY block's state write (see `process_message_for_
        // qso`'s `QsoState::SendingReport` fix-up): several transitions —
        // `Completed` above all — are built from the PRECEDING state's own
        // `frequency` field, so updating only `metadata.frequency` would log
        // the pre-switch offset. `set_frequency` is a no-op on terminal and
        // `Idle` states by design.
        progress.state.set_frequency(applied_hz);
        // ...and because `QsoState::frequency()` is dual-purposed, that write
        // alone is not enough. Whenever `metadata.partner_freq` is `None`, the
        // state's own frequency is ALSO the RX-side baseline every relevance/
        // routing and drift gate keys on (`partner_freq.unwrap_or(qso_freq)` in
        // `is_message_relevant`, `classify_relevance` and
        // `maybe_confirm_frequency_drift_at`). This switch moves only OUR side:
        // the DX is still transmitting on the old offset. Latching the offset
        // we moved AWAY from into `partner_freq` keeps those gates pointed at
        // the DX while `metadata.frequency`/the state frequency track our new
        // TX offset — the same bookkeeping `compute_manual_tx_offset` performs
        // for the analogous case (`partner = (tx_off != dx_freq).then_some(
        // dx_freq)`), and it borrows that site's float-noise tolerance
        // ([`TX_OFFSET_NOOP_TOLERANCE_HZ`]) so a no-op switch never manufactures
        // a split — belt and braces now that guard 3 above refuses such a
        // switch outright. Without it the DX's
        // real replies fall outside `ESTABLISHED_FREQ_TOLERANCE_HZ` of our new
        // offset and stop routing to this QSO altogether, and the drift gate
        // then reads the DX's unchanged frequency as a confirmed drift and
        // relatches us straight back onto the offset the switch existed to
        // escape.
        //
        // An already-`Some` `partner_freq` (Hound, an offset hold, a collision
        // nudge, a passband clamp) is deliberately left alone: it is already
        // the correct "where we hear the DX", and this is purely a TX-side
        // move.
        //
        // The latch also requires an ESTABLISHED DX identity
        // (`their_callsign().is_some()` — the same discriminator
        // `is_message_relevant`/`classify_relevance` use to pick between
        // `FREQ_TOLERANCE_HZ` and `ESTABLISHED_FREQ_TOLERANCE_HZ`). On an
        // unanswered `CallingCq` there is no DX at the old offset at all: it
        // was OUR abandoned CQ frequency. Latching it would aim the RX gates
        // at dead air, and — being pre-establishment — they would judge a
        // caller answering us on our REAL (new) offset against it with the
        // tight 15 Hz bound, rejecting every answer for the life of the QSO
        // and spawning a duplicate QSO object in its place.
        if progress.metadata.partner_freq.is_none()
            && progress.state.their_callsign().is_some()
            && (applied_hz - old_off).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ
        {
            progress.metadata.partner_freq = Some(old_off);
        }
        // `info!`, not `warn!`: the removed hop fired only on an identical-
        // repeat trigger and was genuinely rare, but the silence-based
        // detector this replaced it with can fire roughly once a minute per
        // stalled QSO. That is routine adaptive operation, not a warning.
        info!(
            target: "tx.freq",
            qso_id = %qso_id,
            dx = progress.metadata.their_callsign.as_deref().unwrap_or("?"),
            "Adaptive TX-offset action: {:.0} Hz -> {:.0} Hz",
            old_off, applied_hz
        );
        // PAN-72 (Codex round 1 on PR #350, finding 7): announce the applied
        // move so the coordinator can rebuild the `ActiveQsosSnapshot` the TUI
        // banner and the remote gateway render from. That snapshot is
        // otherwise rebuilt only on a state transition, and a stalled exchange
        // is precisely the case where no further transition arrives — the UI
        // would show the pre-switch offset through several switch/revert
        // cycles. Emitted AFTER the write guard is dropped, matching every
        // other emit site in this file.
        drop(qsos);
        self.emit_event(QsoEvent::TxOffsetApplied {
            qso_id,
            offset_hz: applied_hz,
        })
        .await;
        Ok(applied_hz)
    }

    /// Get next contest serial number
    pub async fn get_next_serial(&self) -> SerialNumber {
        let mut next_serial = self.next_serial.write().await;
        let serial = *next_serial;
        *next_serial += 1;
        serial
    }

    // Internal helper methods

    /// Render a `MessageType` to the FT8 text we would transmit, so the
    /// recorded `Sent` `QsoMessage.raw_text` matches what goes on the air.
    /// Without this, engine-emitted Sent records carried an empty string and
    /// the TUI "TX:" line was blank (UX audit Batch 2). Falls back to an
    /// empty string on the (unexpected) render error rather than failing the
    /// QSO; the message still transmits via the separate encode path.
    fn render_sent_text(&self, message: &MessageType) -> String {
        crate::exchange::MessageExchange::new(self.config.our_callsign.clone())
            .generate_message(message)
            .unwrap_or_default()
    }

    async fn process_message_for_qso(
        &self,
        qso_id: QsoId,
        message: QsoMessage,
        observed_slot_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<(), QsoManagerError> {
        let mut pending_rejections = Vec::new();
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;

        let old_state = progress.state.clone();

        // --- First-decode parity refinement ---
        // This QSO's `tx_parity` may have been latched PROVISIONALLY: when
        // `respond_to_cq_with`/`respond_to_caller` answered a DX-cluster/
        // DX-Hunter spot with no observed live decode yet, there was no real
        // DX parity to oppose, so the latch fell back to a "nearest next
        // slot" guess (`tx_parity_provisional == true`). The first time a
        // frame genuinely FROM that latched partner arrives carrying a
        // determinable slot parity, refine the latch to the TRUE
        // opposite-of-DX parity. Sender verification reuses the same
        // `is_partner` check the rest of this function relies on elsewhere —
        // never weakened for this purpose. This runs exactly once (the flag
        // flip prevents re-running) and emits no event of its own; it must
        // happen BEFORE `qso_tx_parity` is captured just below so a reply
        // this SAME call emits already carries the refined value.
        if progress.metadata.tx_parity_provisional {
            if let (Some(sender), Some(latched), Some(observed_parity)) = (
                message.message_type.sender_callsign(),
                progress.metadata.their_callsign.as_deref(),
                observed_slot_parity,
            ) {
                if Self::is_partner(sender, latched) {
                    progress.metadata.tx_parity = Some(observed_parity.opposite());
                    progress.metadata.tx_parity_provisional = false;
                }
            }
        }

        // Capture per-QSO routing data while we hold the write lock so the
        // reply emission below does not need to re-acquire it (which would
        // deadlock against this guard).
        let mut qso_frequency = progress.metadata.frequency;
        let qso_tx_parity = progress.metadata.tx_parity;
        let qso_remote_origin = progress.metadata.remote_origin;
        let qso_initiated_by = progress.metadata.initiated_by;
        // PR #344 round-1 Codex P2: a natively-typed ContestReply (PAN-51 --
        // ft8_message_to_qso_type classifies a ReplyWithR decode directly,
        // never routing through the NonStandard reclassification below that
        // used to be the only place this QSO-engagement gate ran) must still
        // be gated the same way, or an ordinary non-contest QSO advances and
        // logs a contest-shaped exchange the moment its real partner happens
        // to send an "R"+grid/report shape.
        let qso_contest_engaged = progress.metadata.contest_info.is_some();
        progress.messages.push(message.clone());

        // Compound-callsign equivalence (catalog C18 / peer D4): if this frame
        // came from our latched partner under a MORE-COMPLETE displayed call
        // (same station per `callsigns_match`, but a longer compound form — e.g.
        // we latched bare `G8BCG` and the DX now signs `EA8/G8BCG`), upgrade the
        // logged `their_callsign` to the fuller form. The compound carries DX /
        // portable info worth preserving in the ADIF.
        //
        // SECURITY (deep audit, Fix 1): the upgrade is gated by
        // `is_safe_compound_upgrade`, NOT a bare `callsigns_match` + length
        // check. `callsigns_match` treats two calls as the same station when
        // they share a base, so an RF attacker who knows the partner's on-air
        // base call could otherwise rewrite the LOGGED callsign by signing a
        // state-appropriate frame with arbitrary junk wrapped around that base
        // (e.g. `BOGUS9/G8BCG/MM`). The safe check additionally requires every
        // extra token to be a recognized prefix/suffix (per the same rules
        // `validate_callsign` uses) and forbids replacing a latched affix with
        // a different one — so only a genuine compound completion of the same
        // base is accepted. A rejected-but-matching upgrade is logged at
        // `warn!(target: "qso.security")`.
        if let (Some(sender), Some(latched)) = (
            message.message_type.sender_callsign(),
            progress.metadata.their_callsign.as_deref(),
        ) {
            if crate::exchange::is_safe_compound_upgrade(latched, sender) {
                let upgraded = sender.to_string();
                info!(
                    target: "qso.compound",
                    from = %latched,
                    to = %upgraded,
                    "upgrading logged partner callsign to more-complete compound form (C18)"
                );
                progress.metadata.their_callsign = Some(upgraded);
            } else if sender.len() > latched.len()
                && crate::exchange::callsigns_match(sender, latched)
            {
                // Same base + strictly longer, but NOT a safe compound
                // completion (junk affix token, or a different prefix/suffix
                // than already latched). Do not overwrite the logged call.
                warn!(
                    target: "qso.security",
                    latched = %latched,
                    rejected = %sender,
                    "rejected unsafe compound-callsign upgrade (unrecognized or substituted affix); keeping latched logged callsign"
                );
                pending_rejections.push(QsoEvent::MessageRejected {
                    qso_id,
                    reason: RejectionReason::UnsafeCompoundUpgrade,
                    from_callsign: Some(sender.to_string()),
                    to_callsign: message
                        .message_type
                        .addressee_callsign()
                        .map(str::to_string),
                });
            }
        }

        // Determine state transition based on current state and message.
        // `initiated_by` is threaded through so the manual-only state-regression
        // arms ("back up to where the DX thinks we are") never fire for
        // autonomous QSOs.
        let new_state = self
            .determine_state_transition(
                qso_id,
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
                qso_contest_engaged,
            )
            .await?;

        // Did this received frame advance the QSO? Computed here (before
        // `old_state`/`new_state` are moved into emit_state_change) for the
        // forward-advance tracking below, which resets `stall_cycles`, bumps
        // `advance_generation` and records `last_known_good_offset_hz` on a
        // real advance.
        //
        // PAN-72 (Codex round 1 on PR #350, finding 2): this uses the
        // role-agnostic `progress_rank`, NOT the Caller-shaped `ladder_rank`
        // the manual context-reply path compares against `ResponseStep`. See
        // `progress_rank`'s doc comment: with `ladder_rank`, the CQer's real
        // `CallingCq -> WaitingForReport` advance read as "no progress", so a
        // stall streak built up over unanswered CQs carried into the
        // established exchange and could trip the switch threshold on the
        // very first missed report slot — with no known-good offset recorded.
        let dx_frame_advanced = Self::progress_rank(&new_state) > Self::progress_rank(&old_state);

        // Hound QSY gate: computed here while both `old_state` and `new_state`
        // are still in scope (before `old_state` is consumed by `emit_state_change`
        // below). The QSY fires after the state-update block where we can access
        // `progress` again.
        let was_responding_to_cq = matches!(old_state, QsoState::RespondingToCq { .. });
        let advanced_to_sending_report = matches!(new_state, QsoState::SendingReport { .. });

        // Auto-sequence the outbound reply for MANUAL-initiated QSOs only.
        // The reply is generated from the SAME (pre-transition state,
        // received message) pair that drove the transition, so the two never
        // disagree. Autonomous-initiated QSOs are deliberately left UNCHANGED
        // (no auto-reply) — that remains gated for Phase 5.
        //
        // `reply_to_emit` is captured here (under the lock) and emitted after
        // the write guard is released, since `emit_event` only needs the
        // broadcast channel, not the QSO map.
        // Detect a manual state regression: the DX sent an EARLIER-stage
        // message and `determine_state_transition` either backed us up the
        // ladder (rank decreased) or kept us in SendingReport on a repeated
        // report. Used to (a) count the re-send against the manual watchdog cap
        // and (b) gate the per-slot rearm so it does not double-send in the
        // same slot.
        let is_manual_regression = qso_initiated_by == CallInitiation::Manual
            && (Self::ladder_rank(&new_state) < Self::ladder_rank(&old_state)
                || (matches!(old_state, QsoState::SendingReport { .. })
                    && matches!(new_state, QsoState::SendingReport { .. })
                    && matches!(message.message_type, MessageType::SignalReport { .. })));

        let mut reply_to_emit: Option<MessageType> = None;
        // Phase 5 (autonomous auto-completion): forward auto-sequencing now fires
        // for BOTH Manual and Auto QSOs — an autonomous-opened pounce / CQ-answer
        // must advance its reply ladder (grid → R-report → RR73 → 73) exactly as a
        // manual QSO does, otherwise the autonomous operator opens a QSO and then
        // goes silent after the opening call. Regression handling stays Manual-only:
        // `is_manual_regression` is always false for an Auto QSO, so an Auto QSO
        // emits a reply ONLY on a genuine FORWARD advance (`new_state != old_state`).
        // This activates only while the autonomous operator is running (Auto QSOs
        // are created solely by `respond_to_cq` / `start_cq`), so it is gated behind
        // the autonomous toggle by construction.
        if new_state != old_state || is_manual_regression {
            let exchange = crate::exchange::MessageExchange::new(self.config.our_callsign.clone());
            match exchange.generate_response(
                &old_state,
                &message.message_type,
                message.signal_strength,
            ) {
                Ok(Some(reply)) => reply_to_emit = Some(reply),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        qso_id = %qso_id,
                        "failed to generate auto-sequence reply: {}",
                        e
                    );
                }
            }
        }

        // UX audit Batch 2 #8: latch the DX's grid the moment it arrives in a
        // CqResponse (the opening "<us> <them> <grid>" / "CQ <them> <grid>"
        // exchange). The common close arm (SendingReport → Completed) hard-codes
        // grid_square: None, so without latching here the decoded grid never
        // reaches the logged ADIF GRIDSQUARE. We only overwrite when the
        // incoming grid is present so a later grid-less message can't clear it.
        if let MessageType::CqResponse {
            grid: Some(grid), ..
        } = &message.message_type
        {
            if !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        // PAN-49: latch the grid the moment a contest "R"+grid ack arrives,
        // mirroring the CqResponse grid-latch above — the transition arm's
        // WaitingForConfirmation.grid_square only feeds the Completed-state
        // latch (see the block below), not metadata directly, and ADIF
        // logging (adif.rs) reads metadata.grids.theirs, not QsoState.
        //
        // Guarded by sender verification against `old_state`'s ORIGINAL
        // partner (final review Finding 1): the transition arm's own
        // `reject_sender` check may have rejected this frame (spoofed or
        // third-party sender) and left `new_state == old_state`, but this
        // latch used to run unconditionally regardless of that outcome —
        // writing an unrelated station's grid into the QSO's ADIF-bound
        // metadata. The CqResponse latch above is protected the equivalent
        // way structurally: `(RespondingToCq, CqResponse)` has a dedicated
        // relevance arm requiring `is_partner` before it is ever reached.
        //
        // Also gated by `qso_contest_engaged` (PR #344 round-1 Codex P2):
        // this block is unconditional on the transition arm actually
        // advancing, so without this an ordinary non-contest QSO's
        // `grids.theirs` could still get overwritten by a stray
        // ContestReply-shaped decode from its real partner even after the
        // transition arm itself correctly declines to advance.
        if let MessageType::ContestReply {
            grid, from_station, ..
        } = &message.message_type
        {
            let from_partner = old_state
                .their_callsign()
                .is_some_and(|partner| Self::is_partner(from_station, partner));
            if qso_contest_engaged && from_partner && !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        if new_state != old_state {
            // CQer flow: the QSO was created by start_cq with their_callsign
            // None (we didn't know who would answer). The moment a state
            // advance reveals the contra callsign (caller answered), latch it
            // so the logged ADIF/worked-station record carries the right call,
            // and register the callsign mapping for relevance/supersede.
            if progress.metadata.their_callsign.is_none() {
                if let Some(call) = new_state.their_callsign() {
                    progress.metadata.their_callsign = Some(call.to_string());
                }
            }

            // If transitioning to Completed, update metadata with signal reports and end time
            if let QsoState::Completed {
                their_report,
                our_report,
                grid_square,
                ..
            } = &new_state
            {
                progress.metadata.reports = SignalReports {
                    sent: Some(*our_report),
                    received: Some(*their_report),
                };
                progress.metadata.end_time = Some(Utc::now());
                // Prefer the grid carried in the Completed state (CQer path
                // threads it through WaitingForConfirmation); otherwise keep
                // whatever was latched from the opening CqResponse above.
                if let Some(grid) = grid_square {
                    progress.metadata.grids.theirs = Some(grid.clone());
                }
            }

            let completed_metadata = if matches!(&new_state, QsoState::Completed { .. }) {
                // `progress.metadata.frequency` is the audio offset within the
                // slot and MUST stay that way for the QSO's entire lifetime --
                // see its own doc comment (PAN-25 round 2: an earlier version
                // of this fix overwrote it in place, which broke
                // `resend_last_tx`/the TX worker's modulation limit for a
                // close-step retry on a completed QSO). The logged RF
                // frequency (dial + offset; WSJT-X logs the actual on-air
                // frequency, not the dial) is stamped separately: into
                // `completed_rf_frequency_hz` (for the recent-completed
                // suppression's band check) and, only in this LOCAL CLONE, into
                // `frequency` for the emitted event (ADIF/DB logging wants the
                // real on-air frequency) -- the stored `progress.metadata`
                // never gets that clone's `frequency` value.
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };

            // C3 fix (watchdog-vs-just-in-time-answer race): mark that this QSO
            // made a FORWARD state advance in the current watchdog cycle. The
            // manual keep-calling watchdog (`check_timeouts_at`) grants a
            // one-pass reprieve to any QSO that advanced since its previous
            // pass, so a just-in-time DX answer arriving in the very slot the
            // call cap trips is NOT thrown away as Failed{Timeout} in the same
            // tick it advanced. This is deliberately NOT a `call_count` reset —
            // the per-QSO cap still bounds total calls across the whole QSO
            // (C12: per-QSO, not per-step), and a QSO that advances once then
            // goes silent still retires at the cap on the NEXT pass (the flag
            // is cleared every watchdog pass). We set it only on a genuine
            // forward advance (not a manual regression — a DX repeating an
            // earlier message must keep counting against the cap so a stuck DX
            // cannot drive an unbounded ping-pong). Auto QSOs do not use the
            // manual watchdog and are unaffected.
            if qso_initiated_by == CallInitiation::Manual && !is_manual_regression {
                progress.metadata.progressed_this_cycle = true;
            }

            // Rearm-coordination fix: a genuine forward advance stamps
            // `last_call_at` exactly like the manual-regression re-send does
            // (below). Without this, the per-slot keep-call rearm
            // (`rearm_manual_calls_at`) has no way to know a response arrived
            // THIS slot — if the DX's decode lands late in the slot, the 5s
            // watchdog tick can re-emit the OLD rung (rearm fires on stale
            // `last_call_at`) in the same slot the response just advanced us
            // to a NEW rung, producing two TransmitRequests for one slot (a
            // redundant re-TX the operator observed). Stamping here suppresses
            // that slot's rearm at the source, the same way the regression
            // stamp already suppresses a double-send on a repeated DX frame.
            if qso_initiated_by == CallInitiation::Manual {
                progress.metadata.last_call_at = Some(message.timestamp);
            }

            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: message.timestamp,
                reason: TransitionReason::MessageReceived(message.message_type.clone()),
            });

            self.emit_state_change(qso_id, old_state, new_state).await;

            // Emit QsoCompleted event so loggers can auto-log the QSO
            if let Some(metadata) = completed_metadata {
                self.emit_event(QsoEvent::QsoCompleted {
                    qso_id,
                    metadata,
                    state_history: progress.state_history.clone(),
                    messages: progress.messages.clone(),
                })
                .await;
            }
        }

        // Count a manual regression re-send against the keep-calling watchdog
        // so a DX that keeps repeating an earlier message cannot drive an
        // unbounded ping-pong. We bump `call_count` and stamp `last_call_at`
        // to `message.timestamp`; the latter also gates `rearm_manual_calls_at`
        // (which only re-emits when ≥1 slot has elapsed since `last_call_at`),
        // so the in-slot transition re-send and the per-slot rearm never both
        // fire in the same slot. `first_call_at` is left untouched — a
        // regression must not reset the watchdog clock.
        if is_manual_regression {
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(message.timestamp);
                // A regression is the opposite of forward progress (the DX
                // repeated an earlier message), so it cancels any pending C3
                // reprieve from an earlier advance — a stuck DX repeating must
                // still retire at the cap, never earn an extra watchdog pass.
                progress.metadata.progressed_this_cycle = false;
            }
        }

        // PAN-72: a genuine forward advance resets the stall streak and
        // records the offset we were on as "known-good" — the offset a
        // future stall-triggered Switch/Revert decision reasons about. The
        // non-advancing case is no longer driven by inspecting incoming
        // frame content here; `QsoManager::rearm_manual_calls_at` is now the
        // sole site that increments `QsoMetadata::stall_cycles` (on silence
        // — a real per-slot re-send with no DX response) and emits
        // `QsoEvent::TxOffsetActionNeeded` once it trips
        // `TimeoutConfig::qso_stall_switch_after`.
        if let Some(progress) = qsos.get_mut(&qso_id) {
            if dx_frame_advanced {
                progress.metadata.stall_cycles = 0;
                // PAN-72 (Codex round 4 on PR #350, finding 3): credit the
                // offset the DX actually heard us on. A relocation is committed
                // AFTER the frame that triggered it has gone out on the old
                // offset, so an advance arriving inside
                // `PRE_SWITCH_OFFSET_GRACE` of that move is answering the
                // PRE-switch transmission — the new offset has not been
                // transmitted on even once. Crediting it would record an
                // unproven offset as known-good and poison the Revert target: a
                // later stall would "revert" to the offset it is already on
                // (refused as a no-op) and the genuinely proven one is lost.
                //
                // Consumed here either way: the first advance after a switch is
                // the one that resolves the ambiguity, and everything after it
                // is unambiguously about the current offset.
                progress.metadata.last_known_good_offset_hz = Some(
                    live_pre_switch_offset_at(&progress.metadata, message.timestamp)
                        .unwrap_or(progress.metadata.frequency),
                );
                progress.metadata.pre_switch_offset = None;
                // PAN-72 (Codex round 1 on PR #350, finding 8): bump the
                // staleness token in lockstep with the two fields above, so a
                // TX-offset action queued BEFORE this advance is recognized as
                // stale when the coordinator's once-per-slot drain finally
                // commits it — see `QsoMetadata::advance_generation`.
                progress.metadata.advance_generation =
                    progress.metadata.advance_generation.saturating_add(1);
            }

            // Hound QSY: when the Fox answers with a signal report
            // (RespondingToCq → SendingReport), the Hound must move its TX
            // offset up into the response region (1000–2700 Hz) and send the
            // R+report (`ReportAck`) there — the defining Hound procedure move.
            //
            // Fires exactly once per QSO (`hound_qsyed` gate). Executes
            // INDEPENDENT of `TxFreqMode` (procedure-mandated, not an
            // autonomous optimisation — unlike the Auto-gated silence stall
            // detector above, which only fires in `TxFreqMode::Auto`).
            //
            // This mutates BOTH `metadata.frequency` (used as `qso_frequency`
            // on the NEXT process_message call) AND `qso_frequency` (rides the
            // ReportAck emitted this cycle) directly, inline, here — unlike the
            // stall detector above, which never mutates frequency itself; it
            // only emits `QsoEvent::TxOffsetActionNeeded` for the coordinator's
            // `apply_tx_offset_switch` to resolve. We also
            // update the frequency field inside the already-set `SendingReport`
            // state so the subsequent `Completed` state (built from
            // `SendingReport.frequency` on the Fox RR73 arm) logs our actual
            // QSY'd TX offset rather than the old low calling offset.
            if progress.metadata.hound
                && !progress.metadata.hound_qsyed
                && was_responding_to_cq
                && advanced_to_sending_report
            {
                let fox_call = progress.metadata.their_callsign.as_deref().unwrap_or("");
                let (resp_min, resp_max) = (
                    self.config.hound.response_min_hz,
                    self.config.hound.response_max_hz,
                );
                let qsy = hound_offset_for(fox_call, resp_min, resp_max);
                let old_off = progress.metadata.frequency;
                progress.metadata.frequency = qsy;
                progress.metadata.pending_freq_drift = None;
                progress.metadata.hound_qsyed = true;
                // PAN-72 (Codex round 1 on PR #350, finding 5): re-anchor the
                // known-good offset onto the POST-QSY offset. The
                // `dx_frame_advanced` block a few lines above just recorded
                // the pre-QSY LOW CALLING offset as known-good (this QSY is
                // driven by the very same RespondingToCq -> SendingReport
                // advance), and this block then moves us into the mandatory
                // response region. Left as-is, a subsequent `SendingReport`
                // stall would read `frequency != last_known_good` as "we're
                // sitting on a previously-switched offset" and emit a
                // `Revert` back to the low calling offset — undoing the
                // procedure-mandated QSY and putting our R+report where the
                // Fox is no longer listening.
                //
                // Re-anchoring (rather than exempting Hound from the
                // mechanism) keeps the stall switch/revert ping-pong working
                // WITHIN the response region for a merely slow Fox, while
                // making the QSY'd offset the thing a later revert returns
                // TO.
                progress.metadata.last_known_good_offset_hz = Some(qsy);
                // Keep the ReportAck emitted this cycle on the QSY'd offset.
                qso_frequency = qsy;
                // Update the SendingReport state's frequency so the Completed
                // arm inherits the correct (QSY'd) TX offset.
                if let QsoState::SendingReport {
                    frequency: ref mut state_freq,
                    ..
                } = progress.state
                {
                    *state_freq = qsy;
                }
                info!(
                    target: "hound",
                    qso_id = %qso_id,
                    fox = %fox_call,
                    "Hound: Fox answered — QSY TX {:.0} Hz -> {:.0} Hz (response region {}–{} Hz), sending R+report on new offset",
                    old_off, qsy, resp_min, resp_max
                );
            }
        }

        self.emit_event(QsoEvent::MessageReceived { qso_id, message })
            .await;

        // Record the auto-sequenced reply as a Sent message (under the lock)
        // so it is available to `resend_last_tx` and the UI snapshot. Render
        // the FT8 text so the TUI "TX:" line shows what we sent (UX audit
        // Batch 2 — was String::new()).
        if let Some(reply) = reply_to_emit.as_ref() {
            let reply_text = self.render_sent_text(reply);
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.messages.push(QsoMessage {
                    timestamp: Utc::now(),
                    direction: MessageDirection::Sent,
                    message_type: reply.clone(),
                    raw_text: reply_text,
                    signal_strength: None,
                    frequency: qso_frequency,
                });
            }
        }

        // Release the QSO map write lock before emitting the reply so the
        // emission path holds no locks (and a future change to send_message-
        // style routing cannot deadlock).
        drop(qsos);

        for event in pending_rejections {
            self.emit_event(event).await;
        }

        // Emit the auto-sequenced reply for manual QSOs. We transmit on the
        // QSO's own frequency and reuse the tx_parity latched at QSO start,
        // exactly as the initial-call MessageToSend does.
        if let Some(reply) = reply_to_emit {
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message: reply,
                frequency: qso_frequency,
                tx_parity: qso_tx_parity,
                remote_origin: qso_remote_origin,
            })
            .await;
        }

        Ok(())
    }

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
    fn is_us(&self, call: &str) -> bool {
        crate::exchange::callsigns_match(call, &self.config.our_callsign)
    }

    /// Is `from` the same station as our latched QSO partner `partner`, allowing
    /// a compound↔base change mid-QSO (catalog C18 / peer D4)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`]. Used in the `from == DX` half of
    /// sender verification so an established QSO does not stall when the DX's
    /// displayed call gains or loses a portable prefix/suffix between frames.
    /// Deliberately conservative: genuinely different calls (`K5ARH`/`K5ARG`)
    /// still mismatch — see `callsigns_match` docs.
    fn is_partner(from: &str, partner: &str) -> bool {
        crate::exchange::callsigns_match(from, partner)
    }

    fn verify_sender(&self, from: &str, partner: &str, to: &str) -> Option<RejectionReason> {
        RejectionReason::classify(Self::is_partner(from, partner), self.is_us(to))
    }

    fn verify_addressee(&self, to: &str) -> Option<RejectionReason> {
        (!self.is_us(to)).then_some(RejectionReason::AddresseeNotUs)
    }

    async fn reject_sender(&self, qso_id: QsoId, from: &str, partner: &str, to: &str) -> bool {
        let Some(reason) = self.verify_sender(from, partner, to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    async fn reject_addressee(&self, qso_id: QsoId, from: &str, to: &str) -> bool {
        let Some(reason) = self.verify_addressee(to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    /// The **responder-flow** ladder, deliberately aligned 1:1 with
    /// [`Self::step_ladder_rank`]'s [`pancetta_core::ResponseStep`] mapping so
    /// an operator's context reply can be compared against where an existing
    /// manual QSO currently sits. It is Caller-role-shaped by construction —
    /// there is no `ResponseStep` for "I called CQ" or "I sent my report as
    /// the CQer", so `CallingCq`/`WaitingForReport` have no rung here and
    /// must not gain one.
    ///
    /// For "did this QSO make forward progress?" use [`Self::progress_rank`]
    /// instead — see its doc comment for why the two ladders are separate.
    fn ladder_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::RespondingToCq { .. } => Some(0),
            QsoState::SendingReport { .. } => Some(1),
            QsoState::WaitingForConfirmation { .. } => Some(2),
            QsoState::SendingConfirmation { .. } => Some(2),
            QsoState::Completed { .. } => Some(3),
            _ => None,
        }
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 2): the **role-agnostic**
    /// forward-progress ladder, covering the CQer flow as well as the Caller
    /// flow. Used solely by `process_message_for_qso`'s `dx_frame_advanced`
    /// predicate, which resets [`QsoMetadata::stall_cycles`] and records
    /// [`QsoMetadata::last_known_good_offset_hz`] on a genuine advance.
    ///
    /// This is deliberately NOT [`Self::ladder_rank`]. That ladder exists to
    /// be compared against a [`pancetta_core::ResponseStep`] in the manual
    /// context-reply path and is Caller-role-shaped for that reason; widening
    /// it would silently change which context replies advance an existing QSO
    /// versus re-send its current outbound. Stall detection needs the
    /// opposite property — every real advance in EITHER role must count —
    /// so it gets its own function.
    ///
    /// The CQer rungs mirror the Caller's stage for stage:
    ///   0. opening sent (`CallingCq` / `RespondingToCq`)
    ///   1. our report is on the air (`WaitingForReport` — the CQer's report
    ///      goes out on the `CallingCq -> WaitingForReport` transition — /
    ///      `SendingReport`)
    ///   2. exchange rogered (`WaitingForConfirmation` / `SendingConfirmation`)
    ///   3. `Completed`
    ///
    /// Without rung 0/1 for the CQer, the real `CallingCq -> WaitingForReport`
    /// advance (a caller finally answering an unanswered CQ) read as "no
    /// progress": the stall streak accumulated during the unanswered CQs
    /// carried straight into the established exchange, so a single missed
    /// report slot could trip the threshold immediately — and without ever
    /// recording the just-proven offset as known-good, so the resulting action
    /// was a blind `Switch` rather than the intended anchored ping-pong.
    ///
    /// `Failed` and `Idle` stay `None`: `None` is ordered below every `Some`,
    /// so a QSO going terminal-`Failed` can never read as an advance.
    fn progress_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. } => Some(0),
            QsoState::WaitingForReport { .. } | QsoState::SendingReport { .. } => Some(1),
            // Not currently constructed by `determine_state_transition`, but
            // ranked here so contest wiring inherits correct stall detection
            // rather than re-introducing this same bug.
            QsoState::Contest(ContestState::ExchangingInfo { .. }) => Some(1),
            QsoState::WaitingForConfirmation { .. } | QsoState::SendingConfirmation { .. } => {
                Some(2)
            }
            QsoState::Completed { .. } => Some(3),
            QsoState::Contest(ContestState::ContestCompleted { .. }) => Some(3),
            QsoState::Idle | QsoState::Failed { .. } => None,
        }
    }

    async fn determine_state_transition(
        &self,
        qso_id: QsoId,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
        contest_engaged: bool,
    ) -> Result<QsoState, QsoManagerError> {
        match (current_state, message_type) {
            // CQ call received response (CQer flow). A station answered our CQ
            // with "<us> <them> <grid>". Verify the response is addressed to us
            // (calling_station == our callsign) before advancing — a spurious
            // CqResponse to another station must not hijack our CQ QSO. We latch
            // their grid here (UX audit Batch 2 #8) so the eventual ADIF carries
            // GRIDSQUARE; the relevance filter already directs only
            // addressed-to-us responses here, but we re-verify for defence.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    grid,
                },
            ) => {
                if self
                    .reject_addressee(qso_id, responding_station, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %calling_station,
                        got_from = %responding_station,
                        "CqResponse not addressed to us ignored — no CQ advance"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: latch the report we're about to send (the reply
                // emitter answers with our SignalReport on this transition)
                // so a repeated CqResponse can re-send an IDENTICAL value —
                // see WaitingForReport::our_report's doc comment. No prior
                // report exists to fall back on here, so the no-SNR default
                // matches every other first-time-computed report in this
                // file (-15).
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: responding_station.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                    their_grid: grid.clone(),
                    our_report,
                })
            }

            // A4 (CQer flow — caller skips the grid): a station answers our CQ
            // with a bare signal report ("<us> <them> -NN") instead of the
            // usual grid frame. On-air this means "I copied you, here's your
            // report" — the caller already has our copy. The protocol-correct
            // next move for us (the CQer) is to send THEM our report, exactly
            // as we would after a grid-bearing CqResponse — so we advance to
            // WaitingForReport (same rung as the CqResponse path) and the reply
            // emitter sends our SignalReport. Without this arm CallingCq had no
            // SignalReport transition and we kept re-CQing forever.
            //
            // Sender-verified like every other arm: the report must be TO us
            // (we don't yet know who will answer our CQ, so any from_station is
            // accepted as the contra). We latch their callsign (from_station)
            // so the QSO carries the right contra call; no grid is available.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if self
                    .reject_addressee(qso_id, from_station, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %to_station,
                        got_from = %from_station,
                        "SignalReport not addressed to us ignored — no CQ advance (A4)"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: same latch as the CqResponse arm above — the reply
                // emitter answers with our SignalReport here too, so the
                // value must be captured now for anti-jitter re-sends.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: from_station.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                    their_grid: None,
                    our_report,
                })
            }

            // A5 (CQer flow — caller closes early): after we (the CQer) sent our
            // report (now WaitingForReport) the caller fires RR73 / a plain 73
            // instead of acking with their R-report. The caller is done — accept
            // the early close, complete, and log. This is the CQer-side mirror
            // of the FIX-2 early-close arm the Caller flow already has
            // (SendingReport → Completed on RR73/73). Without it a
            // WaitingForReport CQer ignored the close and the QSO never
            // completed.
            //
            // Sender-verified (from == caller && to == us). We never received a
            // numeric report-ack from them, so log with our computed report and
            // a defaulted their_report.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in WaitingForReport ignored (CQer, A5)"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                let duration = (Utc::now() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // SM-F4: we (the CQer) sent our SignalReport on the CallingCq →
            // WaitingForReport transition, but the caller never copied it and
            // repeats their opening CqResponse (their grid again). There was
            // previously NO arm at all for (WaitingForReport, CqResponse) — it
            // fell through to the no-op default and the QSO silently died at
            // the 30s report_timeout even though the caller was still there.
            // STAY in WaitingForReport (do not advance) and re-send the
            // IDENTICAL `our_report` we already latched — mirrors REGRESSION
            // 2's anti-jitter principle (recomputing from this decode's SNR
            // would jitter the report across repeats). exchange.rs has no
            // (WaitingForReport, CqResponse) reply arm, so returning
            // WaitingForReport here is a no-op for the in-slot reply emitter;
            // the rearm (`rearm_manual_calls_at`) owns the actual re-send.
            //
            // Keep the ALREADY-latched grid: we've already answered with a
            // report, not a grid, so a repeat carrying a (possibly different)
            // grid doesn't need to overwrite what we logged the first time.
            // Sender-verified exactly like the original CallingCq + CqResponse
            // arm (calling_station must be us; the responding_station must be
            // the SAME caller we already latched, not a new one hijacking the
            // slot).
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    their_grid,
                    our_report,
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForReport ignored — no regression (SM-F4)"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "CQer QSO staying in WaitingForReport \
                     (caller repeated their grid; they never copied our report)"
                );
                Ok(QsoState::WaitingForReport {
                    their_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: *started_at,
                    their_grid: their_grid.clone(),
                    our_report: *our_report,
                })
            }

            // CQer flow: we sent our SignalReport (on the CallingCq→
            // WaitingForReport transition) and the caller rogered it with their
            // R-report (ReportAck). Advance to WaitingForConfirmation; the reply
            // emitter answers our FinalConfirmation (RR73). Carry the latched
            // grid into the confirmation state so it reaches Completed/ADIF.
            // Sender-verified (from == DX && to == us) like every other arm.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    their_grid,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in WaitingForReport ignored (CQer)"
                    );
                    return Ok(current_state.clone());
                }
                // The caller's R-report is their report OF US. Our report (of
                // them) was computed when we sent it; recover it from SNR or
                // fall back to the report they just acked.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: their_grid.clone(),
                    started_at: Utc::now(),
                })
            }

            // STUCK-AT-GRID FIX: we answered a CQer with our grid (now in
            // RespondingToCq) and the DX returns OUR call — either a bare
            // "<us> <DX>" or a "<us> <DX> <grid>" (a CqResponse directed at
            // us, carrying no report). On-air this means "I copied you, here
            // I am" — the DX heard our grid. The protocol-correct next move is
            // for us (the answering station) to send the DX a signal report,
            // advancing the contact. Without this arm we re-sent our grid every
            // slot until the manual watchdog timed out — the single
            // highest-frequency stall in the on-air log (N8ME, F5NNN, N9FME,
            // IQ0VT, KB5YNF, KA0NC, first-K9HJZ).
            //
            // A bare-call or grid answer to us parses as a CqResponse with
            // calling_station = us, responding_station = DX. Verify both
            // directions (from DX, to us) before advancing, exactly as on every
            // other arm. We carry no report from the DX yet (their_report:
            // None) and compute OUR report from the SNR.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, target_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in RespondingToCq ignored — sender/target mismatch"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                info!(
                    target: "qso.advance",
                    their_callsign = %target_callsign,
                    "DX returned our call without a report — advancing grid -> signal report"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: None,
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // Response to CQ, waiting for report
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                // Use received signal strength (SNR) as our report, default to received report
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // Phase-5 skip-rung: the DX skipped the plain-report rung and sent an
            // R-report (ReportAck) directly while we are still at grid
            // (RespondingToCq). Treat it like the (WaitingForReport, ReportAck)
            // close for the CQer role: advance to WaitingForConfirmation; the
            // reply emitter sends our RR73, and the (WaitingForConfirmation,
            // RR73/73) arm completes + logs on the DX's roger. Sender-verified
            // exactly as the SignalReport arm above.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: Utc::now(),
                })
            }

            // PAN-49 skip-rung: a state-QSO-party / ARRL Intl Digital
            // partner acks our grid with "R"+grid instead of a numeric
            // report — same close-shape as the ReportAck skip-rung arm
            // above (RespondingToCq -> WaitingForConfirmation), but with no
            // real dB value to carry. `-15` is the established "no real
            // report" sentinel used throughout this file (e.g. lines 3238,
            // 3544, 10260) for exactly this situation. `is_ack: false`
            // (the plain first-exchange grid, already handled by the
            // ordinary CqResponse path) never reaches this arm: both
            // producers of `ContestReply` -- process_message_with_parity's
            // text reclassification (PAN-49) and ft8_message_to_qso_type's
            // direct typed classification (PAN-51) -- only ever set
            // `is_ack: true`.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ContestReply {
                    from_station,
                    to_station,
                    grid,
                    is_ack: true,
                },
            ) => {
                // PR #344 round-1 Codex P2: PAN-51 lets a native ReplyWithR
                // decode reach this arm directly, bypassing the QSO-
                // engagement check process_message_with_parity's text-
                // reclassification path applies before ever producing
                // ContestReply. Without this, an ORDINARY non-contest QSO
                // advances and logs a contest-shaped exchange the moment
                // its real partner sends an "R"+grid shape for any reason.
                // `contest_engaged` is this QSO's own
                // `metadata.contest_info.is_some()`, captured by the caller
                // -- scoped per-QSO, matching the reclassification gate's
                // own scoping (final review Finding 2 there).
                if !contest_engaged {
                    debug!(
                        target: "qso.security",
                        qso_id = %qso_id,
                        from = %from_station,
                        "native ContestReply in RespondingToCq ignored — QSO is not contest-engaged"
                    );
                    return Ok(current_state.clone());
                }
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ContestReply in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: Some(grid.clone()),
                    started_at: Utc::now(),
                })
            }

            // qso-state-machine-analysis GAP-1: the DX skips BOTH remaining
            // rungs and closes directly from our opening grid (RespondingToCq)
            // with RR73/73 — they copied us on the first exchange and are
            // impatient. Mirrors the FIX-2 early-close below (SendingReport)
            // and the A5 early-close (WaitingForReport), which is asymmetric
            // without this arm: RespondingToCq had no early-close, so this
            // exchange previously stalled at grid until the watchdog retired
            // it. their_report is unknown (we never received one) — default to
            // -15 like every other early-close; our_report is best-effort from
            // this closing frame's SNR (we never got a chance to compute it
            // any other way, matching the skip-rung arm above).
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in RespondingToCq ignored"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::Completed {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: 0,
                })
            }

            // Received report acknowledgment
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck ignored"
                    );
                    return Ok(current_state.clone());
                }
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: Utc::now(),
                })
            }

            // FIX 2: the DX rogered our R-report directly with RR73 (or a
            // plain 73). Real FT8 is a 4-message QSO and RR73 is the close,
            // so we must complete (and the reply emitter answers our 73).
            // Without this arm the QSO stalled one message short — the DX's
            // RR73 was ignored and the contact was never logged. We accept
            // both FinalConfirmation (RR73/RRR-class close) and a bare
            // SeventyThree (73) here.
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in SendingReport ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (Utc::now() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // Received final confirmation
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    grid_square,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious FinalConfirmation ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (Utc::now() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // === STATE REGRESSION (manual-initiated QSOs only) ===========
            // Operator principle: "if a DX station re-sends something EARLIER
            // in the conversation, they obviously didn't receive our response —
            // back ourselves up to where THEY think we are."
            //
            // These arms are gated on CallInitiation::Manual so autonomous
            // QSOs are unaffected. Sender verification (from == DX && to == us)
            // is preserved on every regression exactly as on forward arms.

            // REGRESSION 1: we sent RR73 (WaitingForConfirmation) but the DX is
            // still sending us their SignalReport — they never copied our R.
            // Back up two steps to SendingReport and re-send our R-report (the
            // reply emitter answers a ReportAck for this (state, msg) pair).
            // Latch the newest report value the DX sent.
            //
            // SM-F6 safety note: this arm resets `started_at: Utc::now()` on
            // every fire, which would let a repeatedly-regressing QSO reset
            // its own timeout clock indefinitely. Deliberately left
            // Manual-only (not extended to Auto) rather than adding a bounded
            // counter — WaitingForConfirmation is also outside SM-F6's scope
            // (only RespondingToCq/SendingReport get Auto resend/regression),
            // and it already has a uniform `confirmation_timeout` bound for
            // BOTH Manual and Auto QSOs (it's not in the Manual keep-call
            // watchdog list), so an Auto QSO here is already safely bounded
            // by that timeout without this arm's help.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) if initiated_by == CallInitiation::Manual => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Recompute our report from the freshest SNR (fall back to the
                // already-latched value), and latch the DX's newest report.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*our_report);
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → SendingReport \
                     (DX repeated their report; they never copied our R)"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // REGRESSION 2: we sent our R (SendingReport) and the DX re-sends
            // their SignalReport — they didn't copy our R. STAY in
            // SendingReport (do not advance); the per-slot rearm
            // (`rearm_manual_calls_at`, FIX 4) keeps re-sending our R-report.
            // We refresh THEIR report to the newest value for the log, but keep
            // OUR sent report LATCHED — the report we give a station must not
            // jitter with per-decode SNR noise when the DX re-requests (observed
            // on-air: R-7 → R-9 → R-6 across repeats of the same DX report).
            // Returning a SendingReport here drives the their-report update
            // without the reply emitter double-sending: exchange.rs has no
            // (SendingReport, SignalReport) response arm, so the in-slot emit
            // path is a no-op and the rearm owns the (now stable-valued) re-send.
            //
            // SM-F6: extended to Auto QSOs too (no `initiated_by` guard) —
            // unlike REGRESSION 1/3, this arm explicitly PRESERVES
            // `started_at` (see below) rather than resetting it, so an Auto
            // QSO stuck here is still bounded by the ordinary 30s
            // report_timeout counted from when SendingReport was first
            // entered; it cannot reset its own clock by looping through this
            // arm. Safe to extend.
            (
                QsoState::SendingReport {
                    their_callsign,
                    our_report,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in SendingReport ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Keep OUR report latched (do NOT recompute from this decode's
                // SNR) — see the comment above; only their_report refreshes.
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report: *our_report,
                    frequency: *frequency,
                    // Preserve started_at so the manual watchdog keeps measuring
                    // from the original QSO start — a regression must not reset
                    // the keep-calling clock.
                    started_at: *started_at,
                })
            }

            // REGRESSION 3: we sent RR73 (WaitingForConfirmation) but the DX
            // re-sends their original grid/call (CqResponse) — they restarted
            // the whole exchange. Back up to RespondingToCq and re-send our
            // grid/call. Only observable when the repeated message parses as a
            // CqResponse directed appropriately for this QSO.
            //
            // SM-F6 safety note: like REGRESSION 1, this arm resets
            // `started_at: Utc::now()` on every fire. Deliberately left
            // Manual-only for the same reason as REGRESSION 1 — it operates
            // on WaitingForConfirmation, which is outside SM-F6's scope and
            // already carries a uniform `confirmation_timeout` bound for both
            // Manual and Auto QSOs.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) if initiated_by == CallInitiation::Manual => {
                // A "DX K5ARH GRID" repeat parses with calling_station = us,
                // responding_station = DX. Verify both directions before
                // regressing so a spurious station cannot reset our QSO.
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → RespondingToCq \
                     (DX restarted the exchange)"
                );
                Ok(QsoState::RespondingToCq {
                    target_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // No state change
            _ => Ok(current_state.clone()),
        }
    }

    async fn find_qsos_for_message(
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
            let now = Utc::now();
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

    fn classify_relevance(
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
            matches_baseline(match_frequency)
                || live_pre_switch_offset_at(metadata, Utc::now()).is_some_and(matches_baseline)
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

    fn is_message_relevant(
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
        if let Some(qso_freq) = state.frequency() {
            let match_freq = metadata.partner_freq.unwrap_or(qso_freq);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            let matches_baseline = |baseline: f64| (baseline - frequency).abs() <= tolerance;
            if !matches_baseline(match_freq)
                && !live_pre_switch_offset_at(metadata, Utc::now()).is_some_and(matches_baseline)
            {
                return false;
            }
        }

        true
    }

    async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
    ) -> Result<bool, QsoManagerError> {
        if !self.config.duplicate_checking.enabled {
            return Ok(false);
        }

        // Check in-memory active/recent QSOs first (case-insensitive key,
        // Batch 2 #7, matching add/remove_callsign_mapping).
        let key = callsign.to_uppercase();
        let qsos_by_callsign = self.qsos_by_callsign.read().await;
        if let Some(qso_ids) = qsos_by_callsign.get(&key) {
            let qsos = self.qsos.read().await;
            let time_window =
                Duration::hours(self.config.duplicate_checking.time_window_hours as i64);
            let cutoff_time = Utc::now() - time_window;

            for &qso_id in qso_ids {
                if let Some(progress) = qsos.get(&qso_id) {
                    if progress.metadata.start_time > cutoff_time {
                        // Check frequency if required
                        if self.config.duplicate_checking.check_frequency
                            && (progress.metadata.frequency - frequency).abs() > 50.0
                        {
                            continue;
                        }

                        return Ok(true);
                    }
                }
            }
        }
        drop(qsos_by_callsign);

        // Also check the persistent database (catches duplicates after restart
        // or after cleanup_completed_qsos has removed them from memory)
        if let Some(ref db) = self.database {
            let now = Utc::now();
            match db
                .check_duplicate(
                    callsign,
                    frequency,
                    now,
                    self.config.duplicate_checking.time_window_hours,
                    self.config.duplicate_checking.check_frequency,
                )
                .await
            {
                Ok(Some(_qso_id)) => {
                    debug!(
                        "Duplicate QSO for {} found in database (not in memory)",
                        callsign
                    );
                    return Ok(true);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        "Database duplicate check failed, relying on in-memory only: {}",
                        e
                    );
                }
            }
        }

        Ok(false)
    }

    async fn add_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // UX audit Batch 2 #7: key the callsign map case-insensitively
        // (uppercase). A case/format mismatch between a DX-Hunter call and a
        // Callers reply for the same station would otherwise defeat supersede
        // and re-spawn a duplicate QSO. Callsigns are conventionally uppercase;
        // normalising here (and at every lookup) makes supersede robust.
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        qsos_by_callsign
            .entry(key)
            .or_insert_with(Vec::new)
            .push(qso_id);
    }

    /// FIX 1: return the id of an ACTIVE (non-terminal), MANUAL-initiated QSO
    /// with `callsign` on the same band as `frequency`, if one exists. Used by
    /// the operator re-call paths so mashing Call/Space on a station already in
    /// progress CONTINUES the one QSO rather than superseding it / spawning a
    /// duplicate. Case-insensitive callsign match (matching the callsign-map
    /// keying); "same band" derived via [`crate::utils::frequency_to_band`]
    /// exactly like [`Self::supersede_active_qsos_for`]. When several match
    /// (shouldn't happen post-FIX-1, but be robust), the most-recently-started
    /// one wins.
    async fn find_active_manual_qso_for(&self, callsign: &str, frequency: f64) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| {
                    let matches = p.state.is_active()
                        && p.metadata.initiated_by == CallInitiation::Manual
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band;
                    matches.then_some((id, p.metadata.start_time))
                })
            })
            .max_by_key(|(_, started)| *started)
            .map(|(id, _)| id)
    }

    /// FIX B: return the id of the most-recently-COMPLETED, MANUAL-initiated
    /// QSO with `callsign` on the same band as `frequency`, if it completed
    /// within `within` of now. Used by [`Self::respond_to_caller`] so a
    /// close-step (`Rr73`/`SeventyThree`) reply for a station we JUST finished
    /// working — e.g. the operator mashing "send 73" after already
    /// completing — resolves to the SAME QSO object (re-key its last frame)
    /// instead of spawning a sibling with a brand-new `QsoId` (the double-73
    /// duplicate-ADIF-entry bug). Same `qsos_by_callsign` + band-match
    /// approach as [`Self::find_active_manual_qso_for`]; when several
    /// completed matches exist, the newest (`completed_at`) wins.
    ///
    /// Delegates to [`Self::find_recently_completed_manual_qso_for_at`] with
    /// the real clock; see that function for the testable, explicit-`now`
    /// variant (mirroring [`Self::check_timeouts_at`]'s pattern) — a 45s
    /// real-time grace window is awkward to exercise with `tokio::time::sleep`
    /// in a unit test.
    async fn find_recently_completed_manual_qso_for(
        &self,
        callsign: &str,
        frequency: f64,
        within: chrono::Duration,
    ) -> Option<QsoId> {
        self.find_recently_completed_manual_qso_for_at(callsign, frequency, within, Utc::now())
            .await
    }

    /// Explicit-`now` variant of [`Self::find_recently_completed_manual_qso_for`]
    /// (for testability — see its doc comment).
    ///
    /// PAN-25 (Codex round-1 review): `frequency` here is an audio offset,
    /// but the COMPLETED QSOs this scans against now have their
    /// `metadata.frequency` stamped with the true RF frequency (dial +
    /// offset) at completion time — `frequency` must be adjusted the same
    /// way for the band comparison below to mean anything.
    async fn find_recently_completed_manual_qso_for_at(
        &self,
        callsign: &str,
        frequency: f64,
        within: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| match &p.state {
                    QsoState::Completed { completed_at, .. }
                        if p.metadata.initiated_by == CallInitiation::Manual
                            && crate::utils::frequency_to_band(
                                p.metadata
                                    .completed_rf_frequency_hz
                                    .unwrap_or(p.metadata.frequency),
                            ) == want_band
                            && now - *completed_at <= within =>
                    {
                        Some((id, *completed_at))
                    }
                    _ => None,
                })
            })
            .max_by_key(|(_, completed_at)| *completed_at)
            .map(|(id, _)| id)
    }

    /// FIX 1: forward position of a [`ResponseStep`] on the responder's FT8 QSO
    /// ladder, aligned with [`Self::ladder_rank`] so the two are directly
    /// comparable. `Grid` → 0 (RespondingToCq), `Report`/`ReportAck` → 1
    /// (SendingReport), `Rr73` → 2 (WaitingForConfirmation), `SeventyThree` → 3
    /// (Completed). Used to decide whether a context reply is AHEAD of an
    /// existing QSO's current stage (→ advance) or at/behind it (→ re-send).
    fn step_ladder_rank(step: pancetta_core::ResponseStep) -> Option<u8> {
        use pancetta_core::ResponseStep;
        Some(match step {
            ResponseStep::Grid => 0,
            ResponseStep::Report | ResponseStep::ReportAck => 1,
            ResponseStep::Rr73 => 2,
            ResponseStep::SeventyThree => 3,
        })
    }

    /// FIX 1: advance an EXISTING manual QSO to the state/outbound implied by
    /// `step`, instead of creating a new QSO. Mirrors the (state, message)
    /// mapping in [`Self::respond_to_caller`] but mutates the existing QSO in
    /// place: sets its state, records the outbound as a `Sent` message, emits
    /// the `MessageToSend`, and — when advancing to `SeventyThree` — stamps the
    /// completion metadata and emits `QsoCompleted` so the contact is logged.
    /// The QSO's latched `tx_parity` and `initiated_by` are preserved (we reuse
    /// what was latched at QSO start). Used when an operator context-replies at
    /// a step ahead of where the QSO currently is.
    async fn advance_existing_qso_to_step(
        &self,
        qso_id: QsoId,
        target: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
        our_report: SignalReport,
        their_report_val: SignalReport,
    ) -> Result<(), QsoManagerError> {
        use pancetta_core::ResponseStep;
        let now = Utc::now();

        let (new_state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => {
                // Grid never ranks ahead of an active QSO, so the caller never
                // routes here; re-send current as a safe fallback.
                return self.resend_last_tx(qso_id).await;
            }
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let is_completed = matches!(new_state, QsoState::Completed { .. });
        let raw_text = self.render_sent_text(&message);

        // Mutate the existing QSO under the write lock, capturing what we need
        // for the emits after the lock is released.
        let emit = {
            let mut qsos = self.qsos.write().await;
            let Some(progress) = qsos.get_mut(&qso_id) else {
                return Err(QsoManagerError::QsoNotFound { qso_id });
            };
            let old_state = progress.state.clone();
            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: now,
                reason: TransitionReason::UserAction,
            });
            progress.messages.push(QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            });
            let tx_parity = progress.metadata.tx_parity;
            let remote_origin = progress.metadata.remote_origin;

            // On completion, stamp reports/end-time and prepare the completed
            // metadata (with the real RF frequency = dial + offset) to log.
            // PAN-25 round 2: stamp `completed_rf_frequency_hz` (for the
            // suppression band-check) and the LOCAL CLONE's `frequency` (for
            // the emitted event) -- never the stored `progress.metadata.frequency`
            // itself, which must stay the audio offset for its whole
            // lifetime; see the matching comment in `process_message_for_qso`.
            let completed_metadata = if is_completed {
                progress.metadata.reports = SignalReports {
                    sent: Some(our_report),
                    received: Some(their_report_val),
                };
                progress.metadata.end_time = Some(now);
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };
            // Layer 2 timeline persistence: capture the timeline as of this
            // mutation while the write lock is still held, for the
            // QsoCompleted emit below (after the lock is released).
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();
            (
                old_state,
                tx_parity,
                remote_origin,
                completed_metadata,
                state_history,
                messages,
            )
        };
        let (old_state, tx_parity, remote_origin, completed_metadata, state_history, messages) =
            emit;

        self.emit_state_change(qso_id, old_state, new_state).await;
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
        })
        .await;
        if let Some(metadata) = completed_metadata {
            self.emit_event(QsoEvent::QsoCompleted {
                qso_id,
                metadata,
                state_history,
                messages,
            })
            .await;
        }
        Ok(())
    }

    /// FIX 3: retire every active (non-terminal) QSO with `callsign` on the
    /// same band as `frequency`, marking each `Failed{Superseded}` and
    /// clearing its callsign mapping. Emits a `StateChanged` per superseded
    /// QSO (terminal Failed → AP/snapshot clears in the coordinator). Called
    /// just before a new manual QSO is created so only the most-recent one
    /// remains active.
    ///
    /// "Same band" is derived from the QSO frequency via
    /// [`crate::utils::frequency_to_band`]. Within a single operating session
    /// every active QSO shares the RF band, so in practice this collapses to
    /// "same callsign"; deriving the band keeps the rule correct should
    /// per-QSO RF frequencies ever be threaded through.
    async fn supersede_active_qsos_for(&self, callsign: &str, frequency: f64) {
        let new_band = crate::utils::frequency_to_band(frequency);
        // Look up case-insensitively (Batch 2 #7) so a case/format mismatch
        // can't leak a duplicate active QSO past supersede.
        let key = callsign.to_uppercase();

        // Collect the QSO IDs to supersede under the read lock, then mutate.
        let to_supersede: Vec<QsoId> = {
            let qsos = self.qsos.read().await;
            let ids = match self.qsos_by_callsign.read().await.get(&key) {
                Some(ids) => ids.clone(),
                None => Vec::new(),
            };
            ids.into_iter()
                .filter(|id| {
                    qsos.get(id).is_some_and(|p| {
                        p.state.is_active()
                            && crate::utils::frequency_to_band(p.metadata.frequency) == new_band
                    })
                })
                .collect()
        };

        for qso_id in to_supersede {
            let old_state_and_metadata = {
                let mut qsos = self.qsos.write().await;
                match qsos.get_mut(&qso_id) {
                    Some(progress) => {
                        let old_state = progress.state.clone();
                        progress.state = QsoState::Failed {
                            reason: QsoFailureReason::Superseded,
                            failed_at: Utc::now(),
                            last_state: Box::new(old_state.clone()),
                        };
                        // Capture metadata before this loop iteration's lock
                        // guard drops (Batch 4, SM-F5 — needed for QsoFailed).
                        // Also capture the timeline (Layer 2 persistence) —
                        // superseded QSOs stay in the map and are picked up
                        // later by `cleanup_completed_qsos`, but emitting it
                        // here too means a persistence subscriber doesn't
                        // have to wait up to an hour for it.
                        Some((
                            old_state,
                            progress.metadata.clone(),
                            progress.state_history.clone(),
                            progress.messages.clone(),
                        ))
                    }
                    None => None,
                }
            };
            if let Some((old_state, metadata, state_history, messages)) = old_state_and_metadata {
                let new_state = self.qsos.read().await.get(&qso_id).map(|p| p.state.clone());
                if let Some(new_state) = new_state {
                    self.emit_state_change(qso_id, old_state.clone(), new_state)
                        .await;
                }
                // SM-F5: also emit QsoFailed alongside StateChanged — see the
                // cancel_qso comment for why (dead priority-scoring backoff
                // consumer; idempotent HashSet::remove on the coordinator
                // side makes emitting both events safe).
                self.emit_event(QsoEvent::QsoFailed {
                    qso_id,
                    reason: QsoFailureReason::Superseded,
                    last_state: old_state,
                    metadata,
                    state_history,
                    messages,
                })
                .await;
                self.remove_callsign_mapping(callsign, qso_id).await;
                info!(
                    "Superseded older active QSO {} with {} on band {} (re-call)",
                    qso_id, callsign, new_band
                );
            }
        }
    }

    async fn remove_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // Match the uppercase keying of `add_callsign_mapping` (Batch 2 #7).
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        if let Some(qso_ids) = qsos_by_callsign.get_mut(&key) {
            qso_ids.retain(|&id| id != qso_id);
            if qso_ids.is_empty() {
                qsos_by_callsign.remove(&key);
            }
        }
    }

    async fn emit_event(&self, event: QsoEvent) {
        // 2026-07-18 operator finding: PTT keyed twice for the same QSO's
        // final 73 (confirmed via YO6BHN and UT7UJ — real rig set_ptt ON
        // events, one FT8 slot [~15s] apart, for the identical message and
        // qso_id), from what should be a single send. Every send site
        // upstream of the coordinator's TX worker has been individually
        // read and ruled out as the origin: the 4 places that construct a
        // `TransmitRequest`, `MessageBus::send_message` (plain point-to-
        // point, no retry/fan-out), `coalesce_backlog_into`'s drain/re-
        // enqueue (only re-enqueues the non-TX message that stops the
        // drain, never a `TransmitRequest`), `rearm_manual_calls_at`
        // (its state match excludes `Completed`), `maybe_auto_resend_73`
        // (needs a fresh directed RR73/RRR decode — none found), and the
        // autonomous engine's own TX path (its runtime gate is seeded
        // from `[autonomous].enabled` and stays closed when that's
        // false). This traces every `MessageToSend` at its true source —
        // inside the state machine itself, the one place not yet ruled
        // out — so the next occurrence shows definitively whether this
        // fires twice for one logical send (root cause is here or
        // upstream of here) or once (root cause is downstream, in event
        // delivery/consumption not yet found). Log-only; no behavior
        // change. Remove once root-caused.
        if let QsoEvent::MessageToSend {
            qso_id,
            ref message,
            ..
        } = event
        {
            info!(
                target: "pancetta::qso.send_diag",
                "emit_event(MessageToSend): qso={} message={:?}",
                qso_id, message
            );
        }
        if let Err(e) = self.event_sender.send(event) {
            warn!("Failed to emit QSO event: {}", e);
        }
    }

    async fn emit_state_change(&self, qso_id: QsoId, old_state: QsoState, new_state: QsoState) {
        self.emit_event(QsoEvent::StateChanged {
            qso_id,
            old_state,
            new_state,
            timestamp: Utc::now(),
        })
        .await;
    }

    async fn cleanup_loop(&self) {
        loop {
            // Check if we should continue
            {
                let interval_guard = self.cleanup_interval.read().await;
                if interval_guard.is_none() {
                    break;
                }
            }

            // Wait for next tick
            {
                let mut interval_guard = self.cleanup_interval.write().await;
                if let Some(ref mut interval_timer) = *interval_guard {
                    interval_timer.tick().await;
                } else {
                    break;
                }
            }

            // Perform cleanup
            self.cleanup_completed_qsos().await;
        }
    }

    async fn timeout_check_loop(&self) {
        let mut interval_timer = interval(TokioDuration::from_secs(5)); // Check every 5 seconds

        loop {
            interval_timer.tick().await;
            // Re-arm manual keep-calling BEFORE the watchdog so a re-call
            // that pushes the count to the cap is still counted, then the
            // watchdog can retire it on the same or next tick.
            self.rearm_manual_calls().await;
            self.check_timeouts().await;
        }
    }

    /// Re-arm manual keep-calling at the current time. See
    /// [`Self::rearm_manual_calls_at`].
    async fn rearm_manual_calls(&self) {
        self.rearm_manual_calls_at(Utc::now()).await;
    }

    /// For every manual-initiated QSO still in `CallingCq`, `WaitingForReport`,
    /// `RespondingToCq`, or `SendingReport` (waiting for the DX to come
    /// back / copy our last frame), re-emit that frame at most once per FT8
    /// slot so the operator keeps calling every slot until the DX answers or
    /// the manual watchdog fires. The TX scheduler downstream resolves slot
    /// parity from the `tx_parity` latched on the QSO, so re-emitting more
    /// often than a slot is harmless, but we gate to ~one per slot to avoid
    /// flooding the bus.
    ///
    /// SM-F6: autonomous (`CallInitiation::Auto`) QSOs in `RespondingToCq` or
    /// `SendingReport` are ALSO re-armed here now, but under a small,
    /// hardcoded cap (`AUTO_RESEND_MAX_CALLS`) distinct from Manual's
    /// operator-supervised `manual_call_max_calls` — see the constant's doc
    /// comment for the safety rationale. `CallingCq` and `WaitingForReport`
    /// remain Manual-only (an Auto QSO doesn't reach those rearm-eligible
    /// states via a self-initiated CQ opening; extending that is a separate,
    /// out-of-scope initiative).
    ///
    /// Re-arming increments `call_count` and updates `last_call_at`; the
    /// watchdog ([`Self::check_timeouts_at`]) reads `call_count` and
    /// `first_call_at` (Manual) or the per-state `report_timeout` (Auto) to
    /// decide when to stop.
    pub async fn rearm_manual_calls_at(&self, now: DateTime<Utc>) {
        // One FT8 slot is 15s; re-arm only when at least a slot has
        // elapsed since the last call to keep ~one call per slot.
        const SLOT_SECONDS: i64 = 15;

        // SM-F6: bounded resend cap for AUTONOMOUS QSOs. This is deliberately
        // NOT `manual_call_max_calls` (25 calls, an operator-supervised,
        // long-running bound) — an Auto QSO is unattended TX and must stay
        // conservative. `call_count` starts at 1 (the opening send), so a cap
        // of 2 allows exactly ONE resend. Combined with the SLOT_SECONDS=15s
        // cadence below, that resend lands around the ~15s mark, safely
        // inside the existing 30s `report_timeout` (see check_timeouts_at's
        // "Phase 5" Auto branch) — we are NOT extending that 30s outer bound,
        // only making use of the window with an actual mid-window resend
        // instead of dead silence. No new config surface.
        const AUTO_RESEND_MAX_CALLS: u32 = 2;

        // Each entry carries the exact MessageType to re-emit so a
        // RespondingToCq QSO re-sends the call (CqResponse) while a
        // SendingReport QSO re-sends our R-report (ReportAck) — FIX 4.
        let mut to_recall: Vec<(
            QsoId,
            MessageType,
            f64,
            Option<pancetta_core::slot::SlotParity>,
            bool,
        )> = Vec::new();

        // PAN-72: TX-offset actions (Switch/Revert) a stall-tripped QSO
        // needs, collected here and emitted after the write lock below is
        // dropped — same reason `to_recall` above is collect-then-emit.
        let mut offset_actions_to_emit: Vec<(QsoId, OffsetAction, u32)> = Vec::new();

        // PAN-72 (Codex round 2 on PR #350, finding 5): read the global TX
        // policy ONCE for this pass. Under `Disabled` the coordinator's
        // `tx_hard_mute_reason` blocks every frame this loop re-emits, so
        // nothing goes on the air and the DX's continued silence is not
        // evidence of anything. See the `tx_policy` field's doc comment.
        let tx_muted = !pancetta_core::TxPolicy::from_u8(
            self.tx_policy.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_any_tx();

        {
            let mut qsos = self.qsos.write().await;
            for (&qso_id, progress) in qsos.iter_mut() {
                let is_manual = progress.metadata.initiated_by == CallInitiation::Manual;
                let message = match &progress.state {
                    // Manual CQ (operator `c`): keep calling CQ every slot
                    // until a station answers (→ WaitingForReport, handled by
                    // the normal sequence) or the watchdog retires us. Manual
                    // only — SM-F6 deliberately does NOT extend autonomous
                    // keep-calling to a self-CQ opening; that's a separate,
                    // out-of-scope initiative (Auto QSOs don't currently
                    // keep-call a CQ this way at all).
                    QsoState::CallingCq { .. } if is_manual => MessageType::Cq {
                        callsign: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SM-F4: the CQer's report was sent on the CallingCq →
                    // WaitingForReport transition; if the caller never copied
                    // it and repeats their grid, re-send the SAME latched
                    // value (no SNR jitter — see the (WaitingForReport,
                    // CqResponse) regression arm's doc comment). Manual only,
                    // for the same reason CallingCq above is Manual only: an
                    // Auto QSO never reaches WaitingForReport via a
                    // rearm-eligible CQ opening.
                    QsoState::WaitingForReport {
                        their_callsign,
                        our_report,
                        ..
                    } if is_manual => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // SM-F6: re-send our call/grid every slot while waiting
                    // for the DX to come back. Now covers BOTH Manual
                    // (keep-calling, long watchdog) and Auto (a single
                    // bounded resend, capped below) — an autonomous pounce
                    // whose CqResponse the DX missed previously got ZERO
                    // re-sends and died silently at the 30s report_timeout.
                    QsoState::RespondingToCq {
                        target_callsign, ..
                    } => MessageType::CqResponse {
                        calling_station: target_callsign.clone(),
                        responding_station: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SendingReport is entered two ways with DIFFERENT last-sent
                    // frames, so the rearm must re-send the SAME rung we're
                    // actually at, not always escalate to R-report:
                    //   - their_report == None: we're at the plain-report rung
                    //     (entered via the stuck-at-grid arm — RespondingToCq +
                    //     CqResponse — or a manual Report-step answer). We sent
                    //     a plain SignalReport (-NN) and have NOT yet heard the
                    //     DX's report, so keep-calling MUST re-send -NN, never
                    //     R-NN — escalating here was the KJ5NJF bug: we'd
                    //     advance our own TX to R-report with zero DX input.
                    //   - their_report == Some: FIX 4 — we sent R and the DX
                    //     re-sent their report (they did not copy our R) —
                    //     re-send our R-report each slot, under the SAME
                    //     watchdog, until the DX advances (RR73) or the
                    //     watchdog retires us.
                    // SM-F6: also covers Auto (a single bounded resend,
                    // capped below) — the second of the two Auto-eligible
                    // states (RespondingToCq, SendingReport) per this fix.
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: None,
                        our_report,
                        ..
                    } => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: Some(_),
                        our_report,
                        ..
                    } => MessageType::ReportAck {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // Any later state: the normal sequence drives the rest.
                    _ => continue,
                };

                // Stop re-arming once the watchdog bound is reached; the
                // watchdog itself will retire the QSO on its own pass.
                // Manual gets the long, operator-supervised bound; Auto gets
                // the small, hardcoded, strictly-bounded cap above (SM-F6).
                let max_calls = if is_manual {
                    self.config.timeouts.manual_call_max_calls
                } else {
                    AUTO_RESEND_MAX_CALLS
                };
                if progress.metadata.call_count >= max_calls {
                    continue;
                }

                let elapsed_since_last = progress
                    .metadata
                    .last_call_at
                    .map(|t| (now - t).num_seconds())
                    .unwrap_or(i64::MAX);
                if elapsed_since_last < SLOT_SECONDS {
                    continue;
                }

                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);

                // PAN-72: this real per-slot re-send IS the silence signal —
                // the DX did not advance the QSO since the last slot, so we
                // count it against the stall streak. A forward advance
                // (`process_message_for_qso`) resets this to 0 elsewhere;
                // this is now the sole increment site (see
                // `QsoMetadata::stall_cycles`'s doc comment).
                //
                // ...unless the TX path is muted (round 2, finding 5). A
                // re-send that never leaves the rig is not a silent on-air
                // cycle, and counting it would move the QSO off a known-good
                // offset on the strength of silence we ourselves caused. The
                // existing count is left INTACT rather than reset: whatever
                // was accumulated came from real transmissions and is still
                // valid evidence once TX resumes.
                if !tx_muted {
                    progress.metadata.stall_cycles =
                        progress.metadata.stall_cycles.saturating_add(1);
                }

                let tx_auto = pancetta_core::TxFreqMode::from_u8(
                    self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
                )
                .allows_auto_change();

                if !tx_muted
                    && tx_auto
                    && progress.metadata.stall_cycles >= self.config.timeouts.qso_stall_switch_after
                {
                    let current = progress.metadata.frequency;
                    let action = match progress.metadata.last_known_good_offset_hz {
                        Some(known_good) if (current - known_good).abs() >= f64::EPSILON => {
                            OffsetAction::Revert {
                                target_hz: known_good,
                            }
                        }
                        _ => OffsetAction::Switch { avoid_hz: current },
                    };
                    progress.metadata.stall_cycles = 0;
                    offset_actions_to_emit.push((
                        qso_id,
                        action,
                        // PAN-72 finding 8: the staleness token, captured
                        // here under the same write lock that raised the
                        // action so it can never disagree with it.
                        progress.metadata.advance_generation,
                    ));
                }

                // Record the re-emitted call as a Sent message so the TUI's
                // last-TX line and activity counter advance during keep-calling
                // (UX audit Batch 2 — the panel previously froze because rearm
                // appended nothing, making keep-calling look like a hang). The
                // raw_text is the rendered FT8 text we put on the air.
                let raw_text = self.render_sent_text(&message);
                progress.messages.push(QsoMessage {
                    timestamp: now,
                    direction: MessageDirection::Sent,
                    message_type: message.clone(),
                    raw_text,
                    signal_strength: None,
                    frequency: progress.metadata.frequency,
                });

                to_recall.push((
                    qso_id,
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                    progress.metadata.remote_origin,
                ));
            }
        }

        for (qso_id, message, frequency, tx_parity, remote_origin) in to_recall {
            debug!(
                "Manual keep-calling: re-emitting {:?} on {:.1} Hz (qso={})",
                message, frequency, qso_id
            );
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
            })
            .await;
        }

        for (qso_id, action, raised_at_generation) in offset_actions_to_emit {
            self.emit_event(QsoEvent::TxOffsetActionNeeded {
                qso_id,
                action,
                raised_at_generation,
            })
            .await;
        }
    }

    /// Layer 2 timeline persistence note (docs/observability-diagnostics-plan.md):
    /// `progress.state_history`/`.messages` are dropped here along with the
    /// rest of `progress` — this WAS the "QSO leaves the active map" discard
    /// site the plan calls out. It's safe now: every path that can make a
    /// QSO terminal (`process_message_for_qso`, `advance_existing_qso_to_step`,
    /// the open-at-close branch of `respond_to_caller`, `cancel_qso`,
    /// `supersede_active_qsos_for`, `check_timeouts_at`) already captured and
    /// emitted the full timeline on `QsoEvent::QsoCompleted`/`QsoFailed` at
    /// the moment the QSO went terminal — well before it ever reaches this
    /// 1-hour-later cleanup. A `QsoLogger` with `persist_qso_timeline` on has
    /// already durably written it by the time this runs.
    async fn cleanup_completed_qsos(&self) {
        let mut qsos = self.qsos.write().await;
        let cutoff_time = Utc::now() - Duration::hours(1); // Keep completed QSOs for 1 hour

        let to_remove: Vec<QsoId> = qsos
            .iter()
            .filter(|(_, progress)| match &progress.state {
                QsoState::Completed { completed_at, .. } => *completed_at < cutoff_time,
                QsoState::Failed { failed_at, .. } => *failed_at < cutoff_time,
                _ => false,
            })
            .map(|(&qso_id, _)| qso_id)
            .collect();

        for qso_id in to_remove {
            if let Some(progress) = qsos.remove(&qso_id) {
                if let Some(callsign) = &progress.metadata.their_callsign {
                    drop(qsos); // Release lock before acquiring another
                    self.remove_callsign_mapping(callsign, qso_id).await;
                    qsos = self.qsos.write().await; // Re-acquire lock
                }
                debug!("Cleaned up QSO: {}", qso_id);
            }
        }
    }

    async fn check_timeouts(&self) {
        self.check_timeouts_at(Utc::now()).await;
    }

    /// Watchdog pass at an explicit time (for testability).
    ///
    /// In addition to the standard per-state timeouts, this enforces the
    /// **manual keep-calling watchdog**: a manual-initiated QSO that is
    /// still in `RespondingToCq` is retired (→ `Failed`/idle, callsign
    /// mapping cleared) once it has either transmitted
    /// `manual_call_max_calls` calls OR `manual_call_watchdog_minutes`
    /// have elapsed since the first call — whichever comes first.
    pub async fn check_timeouts_at(&self, now: DateTime<Utc>) {
        let mut qsos = self.qsos.write().await;
        let mut timeouts = Vec::new();

        for (&qso_id, progress) in qsos.iter_mut() {
            let active_tx_state = matches!(
                progress.state,
                QsoState::CallingCq { .. }
                    | QsoState::RespondingToCq { .. }
                    | QsoState::SendingReport { .. }
                    | QsoState::WaitingForReport { .. }
                    | QsoState::WaitingForConfirmation { .. }
                    | QsoState::SendingConfirmation { .. }
            );

            // PAN-17: an outbound message that embeds a callsign the FT8
            // encoder can never represent (neither pack28's fixed 6-char
            // scheme nor the i3=4 nonstandard-callsign hash/58-bit path —
            // see `callsign_is_wire_representable`) would otherwise re-arm
            // and silently fail to transmit every slot for the full
            // watchdog window before self-retiring as a plain Timeout,
            // indistinguishable from "the DX never answered" (the live
            // incident this ticket fixes). Encoding is a pure function of
            // the callsign text, so there is nothing to gain by waiting or
            // retrying — retire on the first watchdog pass that observes
            // it, with a reason distinct from Timeout so the operator sees
            // "cannot transmit this message" rather than a bogus no-reply.
            //
            // Round 2 (Codex review #248, finding 4): checks BOTH sides of
            // the exchange, not just the DX's. Every directed frame also
            // embeds `config.our_callsign` — `StationConfig` permits an
            // arbitrarily long compound call there with no length cap, so a
            // misconfigured `our_callsign` (e.g. `VK9/W1XYZ/MM`, >11 chars)
            // would otherwise leave every QSO against a perfectly normal DX
            // stuck on the generic timeout — the exact PAN-17 symptom just
            // moved to the other callsign.
            //
            // Round 3 (Codex re-review, finding 1): ALSO fast-fails a
            // report-bearing rung (SendingReport / WaitingForReport — see
            // `report_stage_partner_callsign`; PAN-27 finding 3 removed
            // WaitingForConfirmation from that set) whose partner isn't
            // plausibly pack28-standard
            // (`callsign_is_plausibly_pack28_standard`), even though the
            // partner's callsign passes the plain wire-representability
            // check above (fits the i3=4 hash field fine). The round-1
            // encoder fix (`pancetta_ft8::encoder::try_encode_nonstandard`'s
            // doc) makes a numeric report/ack to such a partner fail
            // LOUDLY rather than silently blanking — correct — but nothing
            // stopped the QSO from re-arming that exact doomed encode every
            // slot once it reached this rung, which is the original PAN-17
            // "burns the watchdog window retrying an unencodable message"
            // symptom relocated to the report stage.
            //
            // Round 2 deliberately omitted this check because a naive
            // version (keyed on `metadata.their_callsign`, the "most
            // complete form ever seen") falsely retired healthy QSOs in
            // `adversarial_compound_calls.rs`'s compound-then-base
            // scenarios. That risk doesn't apply here: `report_stage_
            // partner_callsign` reads the STATE's own `their_callsign`
            // field, which is exactly the value `MessageExchange::
            // generate_response` embeds in the real outgoing
            // SignalReport/ReportAck (`to_station`/`from_station` come
            // straight from this same field — see exchange.rs) — so this
            // check fires if and only if the message the QSO is actually
            // about to (re-)send is genuinely unencodable, not on a
            // heuristic guess. `compound_first_then_base_completes` and
            // `cqer_caller_compound_then_base_completes` were updated to
            // assert the (now correct) fast MessageUnencodable retirement
            // — those scenarios need a real numeric report addressed to a
            // compound-form callsign, which round 1's encoder fix already
            // made impossible over real FT8; the tests just never
            // exercised the real encoder before, so they didn't notice.
            if active_tx_state {
                let our = self.config.our_callsign.as_str();
                let their = progress.metadata.their_callsign.as_deref();

                let unencodable_reason = if !callsign_is_wire_representable(our) {
                    Some(format!(
                        "our configured callsign '{}' cannot be represented in any FT8 message format",
                        our
                    ))
                } else if let Some(their) = their {
                    if !callsign_is_wire_representable(their) {
                        Some(format!(
                            "callsign '{}' cannot be represented in any FT8 message format",
                            their
                        ))
                    } else if let Some(report_partner) =
                        report_stage_partner_callsign(&progress.state)
                    {
                        if !callsign_is_plausibly_pack28_standard(report_partner)
                            || !callsign_is_plausibly_pack28_standard(our)
                        {
                            Some(format!(
                                "cannot send a numeric report/ack to '{}' — i3=4 (required \
                                 because a compound callsign is involved) has no field for one",
                                report_partner
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(reason) = unencodable_reason {
                    timeouts.push((qso_id, QsoFailureReason::MessageUnencodable(reason)));
                    continue;
                }
            }

            // Repetitive-TX watchdog (operator request): if a QSO has sat in the
            // same active TX state — i.e. we've been re-sending the SAME message
            // without the DX advancing us — longer than repetitive_tx_timeout_secs,
            // retire it. Applies to BOTH manual and auto QSOs and is checked first
            // (of the time-based watchdogs) so it bounds "stuck sending the same
            // thing" even while the manual keep-call watchdog (below) would
            // otherwise keep re-arming. A forward state advance resets the
            // state's `started_at`, so a healthy, progressing QSO never trips this.
            if active_tx_state {
                if let Some(dur) = progress.state.state_duration(now) {
                    if dur.num_seconds() > self.config.timeouts.repetitive_tx_timeout_secs as i64 {
                        timeouts.push((qso_id, QsoFailureReason::Timeout));
                        continue;
                    }
                }
            }

            // Manual keep-calling watchdog. Covers CallingCq (operator `c`:
            // re-calling CQ until someone answers), RespondingToCq
            // (re-calling the DX), SendingReport (FIX 4: re-sending our
            // R-report when the DX repeats their report), and WaitingForReport
            // (SM-F4: re-sending our report when a caller repeats their grid
            // because they never copied it). In all phases the operator is
            // actively keep-calling, and `call_count` / `first_call_at` span
            // the whole QSO, so the 10-calls / 5-min bound applies to the QSO
            // as a whole. Once the DX advances past these states (a caller
            // answers / ReportAck / RR73 received), the normal state timeouts
            // take over.
            if progress.metadata.initiated_by == CallInitiation::Manual
                && matches!(
                    progress.state,
                    QsoState::CallingCq { .. }
                        | QsoState::RespondingToCq { .. }
                        | QsoState::SendingReport { .. }
                        | QsoState::WaitingForReport { .. }
                )
            {
                let max_calls = self.config.timeouts.manual_call_max_calls;
                let watchdog =
                    Duration::minutes(self.config.timeouts.manual_call_watchdog_minutes as i64);
                let elapsed = progress
                    .metadata
                    .first_call_at
                    .map(|t| now - t)
                    .unwrap_or_else(Duration::zero);

                // C3 race guard: if this QSO made a forward state advance in the
                // current watchdog cycle (the DX just answered), grant a
                // one-pass reprieve — do NOT retire it in the same tick it
                // advanced. The flag is consumed (cleared) here, so a QSO that
                // advanced once then goes silent is retired on the NEXT pass
                // once it re-hits the cap (per-QSO bound preserved; see C12).
                let progressed = progress.metadata.progressed_this_cycle;
                progress.metadata.progressed_this_cycle = false;

                // The call-count cap exists to stop pounding a DX that never
                // answered our INITIAL call (CallingCq / RespondingToCq). Once
                // the DX has ENGAGED — we received their report and advanced to
                // SendingReport — they can clearly hear us, so abandoning the
                // exchange at the call cap is wrong: it can drop the QSO in the
                // very window the DX's closing RR73 arrives. (Observed on-air
                // 2026-06-22, 9K2MP: the 10-call cap retired the QSO at 00:07:29,
                // one slot before its RR73 at 00:07:44 — so the RR73 had no
                // active QSO to complete and the operator had to close it by
                // hand.) In SendingReport, only the (longer) time watchdog and
                // the DX's own messages drive the QSO; the call cap does not
                // apply.
                //
                // SM-F4: WaitingForReport belongs with SendingReport here, NOT
                // with CallingCq/RespondingToCq — a CQer in WaitingForReport
                // has already been ANSWERED (a station replied to our CQ and
                // we've already sent them our report); this is the identical
                // "DX has engaged us" situation as SendingReport, just on the
                // CQer ladder rather than the Caller ladder. Applying the
                // call-count cap here would risk the same 9K2MP-style
                // premature drop for a CQer whose caller is slow to close, so
                // WaitingForReport is likewise exempt from the cap and
                // governed only by the (longer) time watchdog.
                let in_initial_call = matches!(
                    progress.state,
                    QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. }
                );
                let call_cap_hit = in_initial_call && progress.metadata.call_count >= max_calls;
                if !progressed && (call_cap_hit || elapsed >= watchdog) {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
                // Manual calls do not use the (much shorter) per-state
                // timeout while keep-calling; the watchdog above governs.
                continue;
            }

            if let Some(duration) = progress.state.state_duration(now) {
                let timeout_seconds = match &progress.state {
                    QsoState::CallingCq { .. } => self.config.timeouts.cq_timeout,
                    QsoState::WaitingForReport { .. } => self.config.timeouts.report_timeout,
                    QsoState::WaitingForConfirmation { .. } => {
                        self.config.timeouts.confirmation_timeout
                    }
                    // Phase 5: an AUTO pounce / CQ-answer that the DX never replies
                    // to must retire so it does not pin `max_concurrent_qsos`
                    // forever. Manual QSOs in these states are governed by the
                    // keep-call watchdog above (which already `continue`d), so these
                    // arms apply only to Auto QSOs. `report_timeout` is the natural
                    // bound — it is how long we wait for the DX's next frame before
                    // giving up on a one-shot autonomous call.
                    QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. } => {
                        self.config.timeouts.report_timeout
                    }
                    _ => continue,
                };

                // Compare as signed seconds: a NEGATIVE elapsed (the state's
                // `started_at` is later than `now`) must never count as a
                // timeout. In production `started_at` and `now` are both the real
                // clock so elapsed is always ≥ 0, but the sim harness anchors its
                // virtual `now` to a slot boundary that can sit slightly behind
                // the real-clock `started_at` the engine stamps — casting a
                // negative `num_seconds()` to `u64` would wrap to a huge value
                // and spuriously retire a just-opened QSO.
                if duration.num_seconds() > timeout_seconds as i64 {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
            }
        }

        for (qso_id, reason) in timeouts {
            if let Some(mut progress) = qsos.remove(&qso_id) {
                let old_state = progress.state.clone();
                progress.state = QsoState::Failed {
                    reason: reason.clone(),
                    failed_at: now,
                    last_state: Box::new(old_state.clone()),
                };
                // Capture metadata before it's consumed below (Batch 4,
                // SM-F5 — needed for QsoFailed).
                let metadata = progress.metadata.clone();
                // Layer 2 timeline persistence: this is the most common
                // "QSO leaves the active map" discard site (every
                // watchdog/timeout retirement runs through here) — capture
                // the timeline before `progress` is dropped at loop end.
                let state_history = progress.state_history.clone();
                let messages = progress.messages.clone();

                drop(qsos); // Release lock before emitting events
                self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                    .await;
                // SM-F5: this is the most common failure producer (every
                // watchdog/timeout retirement runs through here) and
                // previously never emitted QsoFailed, so the coordinator's
                // priority-scoring failure backoff (`record_failure`) never
                // fired for a timed-out QSO. `reason` here is
                // QsoFailureReason::Timeout for every timing-based push site
                // above, or QsoFailureReason::MessageUnencodable for the
                // PAN-17 unencodable-callsign check.
                self.emit_event(QsoEvent::QsoFailed {
                    qso_id,
                    reason,
                    last_state: old_state,
                    metadata,
                    state_history,
                    messages,
                })
                .await;

                if let Some(callsign) = &progress.metadata.their_callsign {
                    self.remove_callsign_mapping(callsign, qso_id).await;
                }

                warn!("QSO timeout: {}", qso_id);
                qsos = self.qsos.write().await; // Re-acquire lock
            }
        }
    }
}

impl Clone for QsoManager {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            qsos: Arc::clone(&self.qsos),
            qsos_by_callsign: Arc::clone(&self.qsos_by_callsign),
            event_sender: self.event_sender.clone(),
            next_serial: Arc::clone(&self.next_serial),
            cleanup_interval: Arc::clone(&self.cleanup_interval),
            database: self.database.clone(),
            dial_frequency_hz: Arc::clone(&self.dial_frequency_hz),
            split_tx_frequency_hz: Arc::clone(&self.split_tx_frequency_hz),
            tx_freq_mode: Arc::clone(&self.tx_freq_mode),
            tx_policy: Arc::clone(&self.tx_policy),
        }
    }
}

// Default implementations removed - using the ones at lines 191-226

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }

    /// Drain currently-buffered events into a Vec.
    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn offset_action_switch_and_revert_are_distinct() {
        let switch = OffsetAction::Switch { avoid_hz: 1500.0 };
        let revert = OffsetAction::Revert { target_hz: 1200.0 };
        assert_ne!(switch, revert);
        assert_eq!(switch, OffsetAction::Switch { avoid_hz: 1500.0 });
    }

    #[test]
    fn admit_idle_admits_any_desire() {
        use pancetta_core::slot::SlotParity::*;
        assert_eq!(admit_new_qso(None, Some(Even)), TxAdmission::Admit);
        assert_eq!(admit_new_qso(None, Some(Odd)), TxAdmission::Admit);
        assert_eq!(admit_new_qso(None, None), TxAdmission::Admit);
    }

    #[test]
    fn admit_same_side_admits_cross_side_queues() {
        use pancetta_core::slot::SlotParity::*;
        // Same side → concurrent (same window) → admit.
        assert_eq!(admit_new_qso(Some(Even), Some(Even)), TxAdmission::Admit);
        assert_eq!(admit_new_qso(Some(Odd), Some(Odd)), TxAdmission::Admit);
        // Cross side → would TX in the opposite window → queue.
        assert_eq!(admit_new_qso(Some(Even), Some(Odd)), TxAdmission::Queue);
        assert_eq!(admit_new_qso(Some(Odd), Some(Even)), TxAdmission::Queue);
    }

    #[test]
    fn admit_unpinned_desire_rides_live_side() {
        use pancetta_core::slot::SlotParity::*;
        // A request that doesn't pin a parity (e.g. CQ, scheduler picks) never
        // conflicts — it rides whatever side is already live.
        assert_eq!(admit_new_qso(Some(Even), None), TxAdmission::Admit);
        assert_eq!(admit_new_qso(Some(Odd), None), TxAdmission::Admit);
    }

    #[tokio::test]
    async fn current_tx_side_none_when_idle_then_pins_after_admit() {
        use pancetta_core::slot::SlotParity;
        let manager = QsoManager::new(test_config());
        assert_eq!(manager.current_tx_side().await, None);

        // A responder latches tx_parity = opposite(dx_parity). DX on Even → we
        // TX Odd, so the side becomes Odd.
        manager
            .respond_to_cq("K9XYZ".to_string(), 14074000.0, Some(SlotParity::Even))
            .await
            .unwrap();
        assert_eq!(manager.current_tx_side().await, Some(SlotParity::Odd));
    }

    #[tokio::test]
    async fn test_start_cq() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager.start_cq(14074000.0, None, false).await.unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::CallingCq { .. }));
        assert_eq!(progress.metadata.frequency, 14074000.0);
    }

    /// docs/qso-engine-bugs.md BUG 1, autonomous path: `SlotParityConfig::Auto`
    /// resolves to `tx_parity: None` for a self-CQ opening (see
    /// `classify_autonomous_opening`), so the AUTONOMOUS CQ path is exposed to
    /// the identical bug as the manual path — `start_cq` must latch a concrete
    /// parity too.
    #[tokio::test]
    async fn autonomous_cq_with_no_parity_preference_latches_a_concrete_parity() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager.start_cq(14074000.0, None, false).await.unwrap();
        assert!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .tx_parity
                .is_some(),
            "start_cq(tx_parity: None) must latch a concrete parity, not leave it None"
        );
    }

    // ── Batch 2: responder-side provisional parity latch + first-decode
    //    refinement ──────────────────────────────────────────────────────────

    /// `respond_to_cq_with` answering a spot with NO observed dx_parity (e.g.
    /// a DX-cluster/DX-Hunter spot never actually decoded live) must latch a
    /// CONCRETE parity immediately, marked provisional — not leave
    /// `tx_parity` permanently `None` (which used to make the TX scheduler
    /// re-resolve "nearest next slot" independently every subsequent slot and
    /// alternate parity).
    #[tokio::test]
    async fn respond_to_cq_with_no_dx_parity_latches_provisional() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_with(
                "K1DEF".to_string(),
                14074000.0,
                None,
                CallInitiation::Manual,
                None,
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.tx_parity.is_some(),
            "must latch a concrete parity, not leave None"
        );
        assert!(
            progress.metadata.tx_parity_provisional,
            "a parity latched with no observed dx_parity must be marked provisional"
        );
    }

    /// `respond_to_caller` answering with NO observed dx_parity (non-Grid
    /// step) latches the same way: concrete + provisional.
    #[tokio::test]
    async fn respond_to_caller_with_no_dx_parity_latches_provisional() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_caller(
                "K1DEF".to_string(),
                14074000.0,
                None,
                pancetta_core::ResponseStep::Report,
                Some(-8.0),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.tx_parity.is_some(),
            "must latch a concrete parity, not leave None"
        );
        assert!(
            progress.metadata.tx_parity_provisional,
            "a parity latched with no observed dx_parity must be marked provisional"
        );
    }

    /// Regression: `respond_to_cq_with` with a REAL observed `dx_parity` sets
    /// `tx_parity_provisional: false` — unchanged behavior for the common
    /// case (a genuine live decode drove the answer).
    #[tokio::test]
    async fn respond_to_cq_with_real_dx_parity_is_not_provisional() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_with(
                "K1DEF".to_string(),
                14074000.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                None,
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.tx_parity,
            Some(pancetta_core::slot::SlotParity::Odd)
        );
        assert!(
            !progress.metadata.tx_parity_provisional,
            "a parity latched from a REAL observed dx_parity must not be provisional"
        );
    }

    /// Regression: `respond_to_caller` with a REAL observed `dx_parity` also
    /// sets `tx_parity_provisional: false`.
    #[tokio::test]
    async fn respond_to_caller_with_real_dx_parity_is_not_provisional() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_caller(
                "K1DEF".to_string(),
                14074000.0,
                Some(pancetta_core::slot::SlotParity::Even),
                pancetta_core::ResponseStep::Report,
                Some(-8.0),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.tx_parity,
            Some(pancetta_core::slot::SlotParity::Odd)
        );
        assert!(
            !progress.metadata.tx_parity_provisional,
            "a parity latched from a REAL observed dx_parity must not be provisional"
        );
    }

    /// First-decode refinement: a QSO answered with no observed dx_parity
    /// (provisional latch) gets its `tx_parity` corrected to the TRUE
    /// opposite-of-DX parity the first time a genuine frame FROM the latched
    /// partner arrives carrying a determinable slot parity — and the
    /// refinement is one-shot (a second frame with a DIFFERENT parity does
    /// NOT change it again).
    #[tokio::test]
    async fn first_decode_from_partner_refines_provisional_parity_once() {
        use pancetta_core::slot::SlotParity;

        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_with(
                "K1DEF".to_string(),
                14074000.0,
                None, // no observed dx_parity — provisional latch
                CallInitiation::Manual,
                None,
                false,
            )
            .await
            .unwrap();
        assert!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .tx_parity_provisional
        );

        // First real frame FROM the latched partner (K1DEF), observed on an
        // Even slot. This must refine tx_parity to Odd (opposite) and clear
        // the provisional flag.
        manager
            .process_message_with_parity(
                MessageType::SignalReport {
                    from_station: "K1DEF".to_string(),
                    to_station: "W1ABC".to_string(),
                    report: -10,
                },
                "W1ABC K1DEF -10".to_string(),
                14074000.0,
                Some(-10.0),
                Some(SlotParity::Even),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.tx_parity,
            Some(SlotParity::Odd),
            "tx_parity must refine to the opposite of the observed DX parity"
        );
        assert!(
            !progress.metadata.tx_parity_provisional,
            "the provisional flag must clear after the first refinement"
        );

        // Second frame from the partner, this time observed on Odd (would
        // imply Even if re-refined) — must NOT change the already-refined
        // parity (one-shot).
        manager
            .process_message_with_parity(
                MessageType::ReportAck {
                    from_station: "K1DEF".to_string(),
                    to_station: "W1ABC".to_string(),
                    report: -10,
                },
                "W1ABC K1DEF R-10".to_string(),
                14074000.0,
                Some(-10.0),
                Some(SlotParity::Odd),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.tx_parity,
            Some(SlotParity::Odd),
            "a second frame must NOT re-refine an already-refined parity"
        );
    }

    /// Security: a frame from a NON-partner (sender mismatch) must NOT
    /// trigger the refinement, even though it carries a determinable slot
    /// parity — sender verification is never weakened for this purpose.
    #[tokio::test]
    async fn refinement_does_not_fire_for_non_partner_sender() {
        use pancetta_core::slot::SlotParity;

        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_with(
                "K1DEF".to_string(),
                14074000.0,
                None, // provisional latch
                CallInitiation::Manual,
                None,
                false,
            )
            .await
            .unwrap();

        // A frame from a DIFFERENT station, addressed to someone else
        // entirely, must not be routed to this QSO at all (find_qsos_for_message
        // relevance gate) and must not refine its provisional parity.
        manager
            .process_message_with_parity(
                MessageType::SignalReport {
                    from_station: "NF4KE".to_string(),
                    to_station: "W1ABC".to_string(),
                    report: -12,
                },
                "W1ABC NF4KE -12".to_string(),
                14074000.0,
                Some(-12.0),
                Some(SlotParity::Even),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.tx_parity_provisional,
            "a non-partner frame must not refine the provisional latch"
        );
    }

    #[tokio::test]
    async fn test_respond_to_cq() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
        assert_eq!(progress.metadata.their_callsign, Some("K1DEF".to_string()));
    }

    // --- active_mode: QsoMetadata.mode follows the station-wide mode -------

    /// Regression: default `QsoManagerConfig` stamps `mode == "FT8"` on every
    /// QSO metadata it creates — byte-identical to pre-FT4 behavior.
    #[tokio::test]
    async fn default_config_stamps_mode_ft8() {
        assert_eq!(QsoManagerConfig::default().active_mode, "FT8");
        let manager = QsoManager::new(test_config());
        // CallingCq metadata (start_cq path).
        let cq_id = manager.start_cq(14074000.0, None, false).await.unwrap();
        assert_eq!(manager.get_qso(cq_id).await.unwrap().metadata.mode, "FT8");
        // RespondingToCq metadata (respond_to_cq path).
        let rx_id = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_eq!(manager.get_qso(rx_id).await.unwrap().metadata.mode, "FT8");
    }

    /// FT4 mode: a manager built with `active_mode = "FT4"` stamps `mode ==
    /// "FT4"` into the metadata of QSOs it creates (→ ADIF `MODE:FT4`).
    #[tokio::test]
    async fn active_mode_ft4_stamps_metadata() {
        let config = QsoManagerConfig {
            active_mode: "FT4".to_string(),
            ..test_config()
        };
        let manager = QsoManager::new(config);
        let cq_id = manager.start_cq(14074000.0, None, false).await.unwrap();
        assert_eq!(manager.get_qso(cq_id).await.unwrap().metadata.mode, "FT4");
        let rx_id = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_eq!(manager.get_qso(rx_id).await.unwrap().metadata.mode, "FT4");
    }

    /// `set_active_mode` updates the manager's configured mode, affecting
    /// only QSOs opened after the call.
    #[tokio::test]
    async fn set_active_mode_affects_qsos_opened_afterward() {
        let mut manager = QsoManager::new(test_config()); // starts at "FT8"
        manager.set_active_mode("FT4".to_string());
        assert_eq!(manager.config().active_mode, "FT4");
    }

    /// An FT4 `QsoMetadata` (built by a manager in FT4 mode) renders ADIF
    /// `MODE:FT4`; the default FT8 manager renders `MODE:FT8` — confirming
    /// `mode` flows through `qso_to_adif` → `generate_record` unchanged.
    #[tokio::test]
    async fn adif_mode_follows_active_mode() {
        use crate::adif::AdifProcessor;
        let processor = AdifProcessor::new();
        for mode in ["FT8", "FT4"] {
            let config = QsoManagerConfig {
                active_mode: mode.to_string(),
                ..test_config()
            };
            let manager = QsoManager::new(config);
            let qso_id = manager
                .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
                .await
                .unwrap();
            let meta = manager.get_qso(qso_id).await.unwrap().metadata;
            let record = processor.qso_to_adif(&meta, None);
            let rendered = processor.generate_record(&record).unwrap();
            assert!(
                rendered.contains(&format!("<MODE:{}>{}", mode.len(), mode)),
                "expected MODE:{mode} in ADIF, got: {rendered}"
            );
        }
    }

    // --- Fix 1 (security): compound-callsign logged-callsign upgrade ------

    /// (a) A bare base latched as the partner, later seen under a SINGLE
    /// standard compound form (recognized country prefix), upgrades the logged
    /// callsign — the legitimate C18 case this feature exists for.
    #[tokio::test]
    async fn compound_upgrade_bare_base_to_standard_compound() {
        let manager = QsoManager::new(test_config()); // our call = W1ABC
        let freq = 14074000.0;
        // We answer a CQ from bare base G8BCG → latched their_callsign=G8BCG.
        let qso_id = manager
            .respond_to_cq("G8BCG".to_string(), freq, None)
            .await
            .unwrap();
        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .their_callsign
                .as_deref(),
            Some("G8BCG")
        );

        // The DX now signs the standard compound EA8/G8BCG in a frame to us.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "EA8/G8BCG".to_string(),
                    report: -12,
                },
                "W1ABC EA8/G8BCG -12".to_string(),
                freq,
                Some(-12.0),
            )
            .await
            .unwrap();

        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .their_callsign
                .as_deref(),
            Some("EA8/G8BCG"),
            "bare base should upgrade to the standard compound form"
        );
    }

    /// (b) An attacker who knows the partner's on-air base call wraps an
    /// ARBITRARY (unrecognized) prefix token around it. Same base → matches,
    /// strictly longer → but the bogus token must NOT be allowed to overwrite
    /// the logged callsign.
    #[tokio::test]
    async fn compound_upgrade_rejects_bogus_affix_token() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager
            .respond_to_cq("G8BCG".to_string(), freq, None)
            .await
            .unwrap();

        // BOGUS9 is 6 chars → not a recognized prefix/suffix token.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "BOGUS9/G8BCG/MM".to_string(),
                    report: -12,
                },
                "W1ABC BOGUS9/G8BCG/MM -12".to_string(),
                freq,
                Some(-12.0),
            )
            .await
            .unwrap();

        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .their_callsign
                .as_deref(),
            Some("G8BCG"),
            "attacker compound with a bogus token must not overwrite the logged callsign"
        );
    }

    /// (c) Already-compound latched call; a frame arrives with the SAME base
    /// but a DIFFERENT recognized prefix (a substitution, not a completion).
    /// It must not silently overwrite the latched call.
    #[tokio::test]
    async fn compound_upgrade_rejects_different_prefix_substitution() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        // Latch the compound EA8/G8BCG directly.
        let qso_id = manager
            .respond_to_cq("EA8/G8BCG".to_string(), freq, None)
            .await
            .unwrap();
        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .their_callsign
                .as_deref(),
            Some("EA8/G8BCG")
        );

        // A frame swaps the prefix to FR (same base, strictly longer string,
        // recognized token — but a substitution of the latched EA8).
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "FR/G8BCG/P".to_string(),
                    report: -12,
                },
                "W1ABC FR/G8BCG/P -12".to_string(),
                freq,
                Some(-12.0),
            )
            .await
            .unwrap();

        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .their_callsign
                .as_deref(),
            Some("EA8/G8BCG"),
            "a different-prefix compound must not replace the latched compound"
        );
    }

    // --- Manual-vs-auto calling semantics (operator policy) --------------

    /// An auto response to a callsign we already have an active QSO with is
    /// rejected by the self-duplicate gate (unchanged behavior).
    #[tokio::test]
    async fn auto_recall_to_same_dx_is_rejected_as_duplicate() {
        let manager = QsoManager::new(test_config());
        let _first = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let second = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(
            matches!(second, Err(QsoManagerError::DuplicateQso { .. })),
            "auto re-call should be a duplicate, got {:?}",
            second
        );
    }

    /// A MANUAL call bypasses the self-duplicate gate even when an active
    /// QSO with that callsign already exists.
    #[tokio::test]
    async fn manual_call_bypasses_duplicate_gate() {
        let manager = QsoManager::new(test_config());
        let _first = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let manual = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(
            manual.is_ok(),
            "manual call must not be blocked by duplicate gate, got {:?}",
            manual
        );
        let progress = manager.get_qso(manual.unwrap()).await.unwrap();
        assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
        assert_eq!(progress.metadata.call_count, 1);
    }

    /// Two consecutive manual calls to the same DX are both allowed (the
    /// operator hit the duplicate-QSO bug doing exactly this).
    #[tokio::test]
    async fn manual_recall_to_same_dx_is_allowed() {
        let manager = QsoManager::new(test_config());
        let a = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        let b = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(a.is_ok() && b.is_ok(), "both manual calls allowed");
    }

    /// FIX 1: re-calling a station with an ACTIVE manual QSO CONTINUES that QSO
    /// — it returns the SAME qso_id, does NOT create a second QSO, and does NOT
    /// supersede (the old QSO is NOT marked Failed{Superseded}). This is the
    /// core of the "mashing Space spawns duplicate/superseding QSOs" fix.
    #[tokio::test]
    async fn manual_recall_of_active_qso_continues_same_qso() {
        let manager = QsoManager::new(test_config());
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let second = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "re-call of an active QSO must continue it (same id), not spawn a new one"
        );

        // Exactly one active QSO for this callsign — the original.
        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            active_for_dx,
            vec![first],
            "exactly one active QSO (the original) should remain"
        );

        // It is NOT superseded — still in its original RespondingToCq state.
        let progress = manager.get_qso(first).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "continued QSO must keep its state, got {:?}",
            progress.state
        );

        // The callsign mapping holds only the one QSO.
        let mapping = manager.qsos_by_callsign.read().await;
        assert_eq!(
            mapping.get("K1DEF").map(|v| v.as_slice()),
            Some([first].as_slice()),
            "mapping must point only to the single continued QSO"
        );
    }

    /// FIX 1 / FIX 3 boundary: a re-call AFTER the prior QSO already went
    /// terminal still works — it creates a FRESH QSO (and supersedes any
    /// lingering terminal record). This is the genuine "work them again" case.
    #[tokio::test]
    async fn manual_recall_after_terminal_creates_fresh_qso() {
        let manager = QsoManager::new(test_config());
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        // Drive the first QSO terminal (operator cancel).
        manager.cancel_qso(first).await.unwrap();

        let second = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_ne!(
            first, second,
            "re-call after the prior QSO went terminal must create a fresh QSO"
        );

        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(active_for_dx, vec![second], "only the fresh QSO is active");
    }

    // --- Manual re-call dedup regression locks (K9HJZ duplicate-QSO incident)
    //
    // Audit (2026-06-17): the manual re-call entry (`respond_to_cq_with`,
    // Manual) is dedup-safe by construction. (1) FIX 1
    // (`find_active_manual_qso_for`) continues an ACTIVE manual QSO instead of
    // spawning a second one. (2) Every terminal transition clears the
    // `qsos_by_callsign` mapping for that QSO — `cancel_qso` and
    // `check_timeouts_at` `qsos.remove` it outright, `supersede_active_qsos_for`
    // marks it `Failed{Superseded}` and removes its mapping — and
    // `find_active_manual_qso_for` / `supersede_active_qsos_for` both filter on
    // `state.is_active()`, so a lingering `Failed` record can neither be
    // "continued" nor block a fresh call. The tests below pin both guarantees
    // against regression. No production change was required.

    /// REGRESSION LOCK (scenario a): a manual re-call to a callsign that already
    /// has an ACTIVE QSO must leave EXACTLY ONE active QSO for that callsign —
    /// never two concurrent ones. Here the prior active QSO is an AUTO QSO (FIX
    /// 1's "continue" only matches a prior *manual* QSO), so the manual re-call
    /// takes the supersede path: the older QSO is retired to
    /// `Failed{Superseded}` and a single fresh manual QSO remains active.
    #[tokio::test]
    async fn manual_recall_supersedes_active_qso_leaving_exactly_one() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;

        // Prior ACTIVE (auto) QSO with the DX.
        let prior = manager
            .respond_to_cq("K9HJZ".to_string(), freq, None)
            .await
            .unwrap();
        assert!(manager.get_qso(prior).await.unwrap().state.is_active());

        // Operator manually re-calls the same DX on the same band.
        let recall = manager
            .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
            .await
            .unwrap();
        assert_ne!(
            prior, recall,
            "a non-manual prior QSO is superseded, not continued — fresh id"
        );

        // The prior QSO is retired (superseded), not still active.
        let prior_state = manager.get_qso(prior).await.unwrap().state;
        assert!(
            matches!(
                prior_state,
                QsoState::Failed {
                    reason: QsoFailureReason::Superseded,
                    ..
                }
            ),
            "prior active QSO must be superseded → Failed, got {:?}",
            prior_state
        );

        // EXACTLY ONE active QSO for the callsign — the new manual one.
        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K9HJZ"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            active_for_dx,
            vec![recall],
            "exactly one active QSO (the fresh manual re-call) must remain"
        );

        // The callsign mapping points only to the surviving QSO — the
        // superseded one was unmapped, so no stale entry lingers.
        let mapping = manager.qsos_by_callsign.read().await;
        assert_eq!(
            mapping.get("K9HJZ").map(|v| v.as_slice()),
            Some([recall].as_slice()),
            "mapping must hold only the surviving QSO, no stale superseded id"
        );
    }

    /// REGRESSION LOCK (scenario b): a manual re-call AFTER the prior QSO with
    /// that callsign was retired by the keep-call WATCHDOG (timed out) must
    /// start a fresh QSO cleanly — no panic, no stale mapping that misroutes or
    /// blocks the new call. This exercises the `check_timeouts_at` terminal
    /// path (distinct from the `cancel_qso` path covered above), which is the
    /// real "frustrated operator re-call" trigger from the K9HJZ incident.
    #[tokio::test]
    async fn manual_recall_after_watchdog_timeout_starts_fresh_cleanly() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 2;
        config.timeouts.manual_call_watchdog_minutes = 60; // call-count is the binding bound
        let manager = QsoManager::new(config);
        let freq = 14074000.0;

        let first = manager
            .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
            .await
            .unwrap();

        // Keep-call until the watchdog cap is reached, then let it retire.
        let mut t = Utc::now();
        for _ in 0..4 {
            t += Duration::seconds(15);
            manager.rearm_manual_calls_at(t).await;
        }
        manager.check_timeouts_at(t).await;

        // The timed-out QSO is gone from the live map AND its callsign mapping
        // is cleared (no stale entry left behind).
        assert!(
            matches!(
                manager.get_qso(first).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "watchdog must have removed the timed-out QSO"
        );
        assert!(
            manager.qsos_by_callsign.read().await.get("K9HJZ").is_none(),
            "timed-out QSO must leave no stale callsign mapping"
        );

        // The operator re-calls the same DX. It must start a fresh, correctly
        // routed QSO — not panic, not be blocked, not be misrouted to the dead id.
        let second = manager
            .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
            .await
            .expect("re-call after watchdog timeout must succeed cleanly");
        assert_ne!(first, second, "must be a brand-new QSO id");

        let progress = manager.get_qso(second).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "fresh QSO must open in RespondingToCq, got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.their_callsign.as_deref(),
            Some("K9HJZ"),
            "fresh QSO must be routed to the re-called DX"
        );
        assert_eq!(
            progress.metadata.call_count, 1,
            "fresh QSO must start its own keep-call count, not inherit the old one"
        );

        // EXACTLY ONE active QSO for the callsign — the fresh one.
        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K9HJZ"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            active_for_dx,
            vec![second],
            "exactly one active QSO (the fresh re-call) after a watchdog timeout"
        );

        // And the mapping points only to it.
        assert_eq!(
            manager
                .qsos_by_callsign
                .read()
                .await
                .get("K9HJZ")
                .map(|v| v.as_slice()),
            Some([second].as_slice()),
            "mapping must point only at the fresh QSO"
        );
    }

    /// FIX 1: a context-Space reply at a step AHEAD of the existing active
    /// QSO's stage ADVANCES the SAME QSO (no new QSO, no supersede). We open a
    /// manual QSO (RespondingToCq → step rank 0), then context-reply at Rr73
    /// (rank 2): the existing QSO must advance to WaitingForConfirmation and
    /// keep its id.
    #[tokio::test]
    async fn context_reply_ahead_advances_existing_qso() {
        use pancetta_core::ResponseStep;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();

        // DX is now ahead (they sent us an R-report); operator context-replies
        // at Rr73. Must advance THIS QSO, not create a new one.
        let advanced = manager
            .respond_to_caller(
                "K1DEF".to_string(),
                freq,
                None,
                ResponseStep::Rr73,
                Some(-10.0),
                Some(-12),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            first, advanced,
            "ahead context reply must continue same QSO"
        );

        let progress = manager.get_qso(first).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "QSO should have advanced to WaitingForConfirmation, got {:?}",
            progress.state
        );

        // Exactly one active QSO for this callsign.
        let active = manager.get_active_qsos().await;
        let n = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .count();
        assert_eq!(n, 1, "exactly one active QSO after the advance");
    }

    /// The manual watchdog retires a RespondingToCq QSO once the call count
    /// reaches `manual_call_max_calls`.
    #[tokio::test]
    async fn manual_watchdog_fires_on_max_calls() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 3;
        config.timeouts.manual_call_watchdog_minutes = 60; // not the binding bound here
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Simulate keep-calling: re-arm enough times (one slot apart) to
        // hit the cap. call_count starts at 1; re-arm to 3.
        let mut t = Utc::now();
        for _ in 0..5 {
            t += Duration::seconds(15);
            manager.rearm_manual_calls_at(t).await;
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.call_count >= 3,
            "expected call_count to reach cap, got {}",
            progress.metadata.call_count
        );

        // Watchdog must now retire it.
        manager.check_timeouts_at(t).await;
        let after = manager.get_qso(qso_id).await;
        assert!(
            matches!(after, Err(QsoManagerError::QsoNotFound { .. })),
            "watchdog should have removed the QSO, got {:?}",
            after.map(|p| p.state)
        );
    }

    /// The manual watchdog retires a RespondingToCq QSO once the elapsed
    /// time exceeds `manual_call_watchdog_minutes`, even below the call cap.
    #[tokio::test]
    async fn manual_watchdog_fires_on_elapsed_time() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000; // not binding
        config.timeouts.manual_call_watchdog_minutes = 5;
        // Isolate the 5-min manual watchdog from the (tighter) repetitive-TX cap.
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Just under 5 minutes: still alive.
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
        manager
            .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
            .await;
        assert!(manager.get_qso(qso_id).await.is_ok());

        // Past 5 minutes: retired.
        manager
            .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));
    }

    /// PAN-17 → PAN-23: a manual call whose DX callsign can never be
    /// encoded onto the FT8 wire (here, the decoder's own hash-miss
    /// placeholder `<...>` — invalid characters, not just "long") must
    /// never even create a QSO in the first place.
    ///
    /// This supersedes PAN-17's original fix, which let `respond_to_cq_manual`
    /// create the QSO and relied on the very next watchdog pass to retire it
    /// with `MessageUnencodable` (consuming a full TX-worker round trip and
    /// briefly showing a doomed QSO in the UI). PAN-23 closes that off at the
    /// door: `respond_to_cq_with`'s `InvalidCallsign` guard rejects the
    /// literal placeholder synchronously, before any `QsoId` is minted or any
    /// `MessageToSend` is emitted — the failure mode observed on-air
    /// (production logs 2026-08-20..22) where the operator's selection led
    /// straight to a guaranteed-to-fail encode attempt.
    ///
    /// The watchdog's fast-retirement guarantee is still exercised
    /// separately by `genuinely_unresolvable_caller_hash_retires_fast_not_a_hang`,
    /// which reaches the same `their_callsign == "<...>"` state through
    /// inbound decode traffic (latched, not requested) — a path this guard
    /// does not and cannot cover, since the placeholder never passes through
    /// `respond_to_cq_with`/`respond_to_caller` as a `target_callsign` there.
    #[tokio::test]
    async fn unencodable_dx_callsign_rejected_immediately_not_via_manual_watchdog() {
        let manager = QsoManager::new(test_config());
        let err = manager
            .respond_to_cq_manual("<...>".to_string(), 14074000.0, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, QsoManagerError::InvalidCallsign { callsign } if callsign == "<...>"),
            "expected InvalidCallsign for the unresolved hash placeholder, got {err:?}"
        );
    }

    /// A genuinely encodable compound callsign (PAN-17's primary fix covers
    /// this on the encoder side) must NOT be caught by the new fast-fail
    /// watchdog — only callsigns that can never be represented on the wire
    /// at all are retired early.
    #[tokio::test]
    async fn encodable_compound_callsign_is_not_retired_by_unencodable_watchdog() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "a genuinely encodable compound callsign must not be retired by the \
             unencodable-message watchdog"
        );
    }

    /// A callsign that's merely long-but-valid up to the 11-char hash-field
    /// limit is representable; only exceeding it (or using an invalid
    /// character) trips the new watchdog.
    #[test]
    fn callsign_is_wire_representable_boundary() {
        assert!(callsign_is_wire_representable("K1ABC")); // plain standard
        assert!(callsign_is_wire_representable("YS/WE9G")); // compound, fits
        assert!(callsign_is_wire_representable("PJ4/KA1ABC")); // 10 chars, fits
        assert!(callsign_is_wire_representable("ABCDEFGHIJK")); // exactly 11
        assert!(!callsign_is_wire_representable("ABCDEFGHIJKL")); // 12: too long
        assert!(!callsign_is_wire_representable("<...>")); // invalid chars
    }

    /// PAN-17 round 2 (Codex review #248, finding 4): the unencodable-
    /// message watchdog only checked `their_callsign`. `StationConfig`
    /// permits an arbitrarily long compound `our_callsign` with no length
    /// cap, so a misconfigured station callsign would leave every QSO --
    /// even against a perfectly normal, plain-callsign DX -- stuck
    /// re-arming an unencodable TX until the slow generic timeout. Must
    /// now retire fast too.
    #[tokio::test]
    async fn unencodable_our_callsign_retires_immediately() {
        let mut config = test_config();
        config.our_callsign = "VK9/W1XYZ/MM/EXTRA".to_string(); // >11 chars
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);
        let mut events = manager.subscribe();

        // The DX side is perfectly ordinary -- proves this is caught even
        // when `their_callsign` alone would pass every existing check.
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));

        let mut reason = None;
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::QsoFailed {
                qso_id: id,
                reason: r,
                ..
            } = ev
            {
                if id == qso_id {
                    reason = Some(r);
                }
            }
        }
        assert!(
            matches!(reason, Some(QsoFailureReason::MessageUnencodable(_))),
            "expected MessageUnencodable, got {:?}",
            reason
        );
    }

    #[test]
    fn callsign_is_plausibly_pack28_standard_boundary() {
        assert!(callsign_is_plausibly_pack28_standard("K1ABC"));
        assert!(callsign_is_plausibly_pack28_standard("W1AW"));
        assert!(callsign_is_plausibly_pack28_standard("K1ABC/P"));
        assert!(callsign_is_plausibly_pack28_standard("K1ABC/R"));
        // Accepted false positive (see the fn's doc): fails pack28's real
        // digit-position rule but this loose check can't detect that.
        assert!(callsign_is_plausibly_pack28_standard("8G81PA"));
        // Unambiguously compound / not pack28-shaped.
        assert!(!callsign_is_plausibly_pack28_standard("YS/WE9G"));
        assert!(!callsign_is_plausibly_pack28_standard("EA8/G8BCG"));
        assert!(!callsign_is_plausibly_pack28_standard("VP2/W1XYZ"));
        assert!(!callsign_is_plausibly_pack28_standard("PJ4/KA1ABC"));
        assert!(!callsign_is_plausibly_pack28_standard("3E40CDW")); // 7 chars
        assert!(!callsign_is_plausibly_pack28_standard("AB")); // 2 chars
    }

    #[test]
    fn report_stage_partner_callsign_extracts_from_report_bearing_states_only() {
        let now = Utc::now();
        assert_eq!(
            report_stage_partner_callsign(&QsoState::SendingReport {
                their_callsign: "YS/WE9G".to_string(),
                their_report: None,
                our_report: -10,
                frequency: 14074000.0,
                started_at: now,
            }),
            Some("YS/WE9G")
        );
        assert_eq!(
            report_stage_partner_callsign(&QsoState::WaitingForReport {
                their_callsign: "YS/WE9G".to_string(),
                frequency: 14074000.0,
                started_at: now,
                their_grid: None,
                our_report: -10,
            }),
            Some("YS/WE9G")
        );
        // PAN-27 finding 3: WaitingForConfirmation is deliberately excluded
        // -- its queued message is always FinalConfirmation (RR73-class),
        // representable via i3=4 regardless of callsign shape. A numeric
        // report only re-enters via a regression to SendingReport, which
        // changes the state variant itself (covered by the arm above).
        assert_eq!(
            report_stage_partner_callsign(&QsoState::WaitingForConfirmation {
                their_callsign: "YS/WE9G".to_string(),
                their_report: -10,
                our_report: -10,
                frequency: 14074000.0,
                grid_square: None,
                started_at: now,
            }),
            None
        );
        // SendingConfirmation only ever sends RR73/73 (both representable
        // via i3=4 regardless of callsign shape) -- deliberately excluded.
        assert_eq!(
            report_stage_partner_callsign(&QsoState::SendingConfirmation {
                their_callsign: "YS/WE9G".to_string(),
                their_report: -10,
                our_report: -10,
                frequency: 14074000.0,
                grid_square: None,
                started_at: now,
            }),
            None
        );
        assert_eq!(report_stage_partner_callsign(&QsoState::Idle), None);
    }

    /// PAN-17 round 3 (Codex re-review of #248, finding 3): a resolved i3=4
    /// hash render ("<K1DEF>") must be normalized to the plain callsign
    /// ("K1DEF") before it flows into latched QSO state -- otherwise the
    /// still-bracketed literal self-sabotages via the unencodable-message
    /// watchdog (`<`/`>` are outside the wire charset). Exercises the full
    /// parse -> process_message -> latch path (the normalization itself
    /// lives in `exchange.rs::normalize_callsign_token`; this proves it's
    /// wired end-to-end into the QSO engine, not just unit-tested in
    /// isolation).
    #[tokio::test]
    async fn resolved_hash_render_reply_latches_plain_callsign_not_bracketed() {
        let mut config = test_config();
        config.our_callsign = "YS/WE9G".to_string(); // compound operator
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager.start_cq(14074000.0, None, false).await.unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        // K1DEF (a standard callsign) replies to our compound CQ; their own
        // callsign lands in the i3=4 hash slot and, having resolved,
        // decodes as "<K1DEF>" -- exactly the wire text
        // `MessageExchange::parse_message` would receive from the decoder.
        let raw_text = "YS/WE9G <K1DEF> EM10";
        let parsed = crate::utils::parse_ft8_message(raw_text, "YS/WE9G").unwrap();
        assert!(
            matches!(&parsed, MessageType::CqResponse { responding_station, .. } if responding_station == "K1DEF"),
            "parse_ft8_message must normalize the resolved hash render, got: {:?}",
            parsed
        );

        manager
            .process_message(parsed, raw_text.to_string(), 14074000.0, Some(-10.0))
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(&progress.state, QsoState::WaitingForReport { their_callsign, .. } if their_callsign == "K1DEF"),
            "expected their_callsign latched as the plain 'K1DEF', got: {:?}",
            progress.state
        );

        // The QSO has landed directly on a report-bearing rung
        // (WaitingForReport) needing a numeric report addressed to OUR
        // OWN compound callsign -- symmetric to finding 1, that
        // genuinely cannot be encoded via real FT8 regardless of which
        // side is compound, so round 3's report-stage check correctly
        // retires it. What this test isolates is WHY: the failure reason
        // must be the report-stage wording (proving K1DEF already passed
        // the plain wire-representability check first) and must NOT
        // mention invalid characters or contain the raw "<" / ">"
        // brackets -- if normalization had NOT happened, the round-2
        // wire-representability check (which runs BEFORE the round-3
        // report-stage check) would have rejected "<K1DEF>" outright on
        // its invalid characters instead.
        let mut events = manager.subscribe();
        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));

        let mut reason = None;
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::QsoFailed {
                qso_id: id,
                reason: r,
                ..
            } = ev
            {
                if id == qso_id {
                    reason = Some(r);
                }
            }
        }
        match reason {
            Some(QsoFailureReason::MessageUnencodable(detail)) => {
                assert!(
                    detail.contains("numeric report"),
                    "expected the report-stage reason, got: {detail}"
                );
                assert!(
                    !detail.contains('<') && !detail.contains('>'),
                    "failure reason must reference the plain callsign, not a \
                     bracketed hash render: {detail}"
                );
                assert!(
                    detail.contains("K1DEF") && !detail.contains("<K1DEF>"),
                    "failure reason must name the plain 'K1DEF', not '<K1DEF>': {detail}"
                );
            }
            other => panic!("expected MessageUnencodable, got {:?}", other),
        }
    }

    /// PAN-17 round 4 (Codex re-review of #248): "resolve first-time
    /// callers before filtering type-4 replies."
    ///
    /// Investigated and determined to be an INHERENT i3=4 protocol
    /// limitation, not a pancetta bug: a 12-bit hash is a one-way
    /// compression of a callsign into 4096 buckets. Reversing it requires
    /// having independently learned the plaintext from a standard-format
    /// decode -- if a station's very first-ever transmission we've heard
    /// (ever, on this band, this session) is itself an i3=4 reply that
    /// puts them in the hash slot (structurally forced whenever the OTHER
    /// party -- us -- is compound, since the pack28-failing callsign
    /// always wins the exact slot), there are no other bits anywhere in
    /// that 77-bit payload encoding their plaintext. Round 3's seeding
    /// (`Ft8Decoder`'s per-window loop over every decoded
    /// `MessageType::Standard`, `pancetta-ft8/src/decoder.rs`) already
    /// seeds from ANY standard-format decode this station makes, whenever
    /// and wherever heard, not just frames addressed to us -- there is no
    /// earlier opportunity pancetta's code is failing to use; WSJT-X has
    /// the identical limitation for the identical reason.
    ///
    /// What this test proves instead: the QSO-layer behavior for this
    /// case is a clean, BOUNDED failure, not a hang or a silent full-
    /// watchdog-window loop. `their_callsign` latches as the literal
    /// unresolved placeholder `"<...>"` (`is_message_relevant`'s
    /// `CallingCq`/`CqResponse` arm does not itself validate the sender's
    /// identity, only that the response is addressed to us -- by design,
    /// since at that point we don't yet know who will answer). The
    /// EXISTING `callsign_is_wire_representable` watchdog check (PAN-17
    /// round 2) then retires it on the very next pass: `"<...>"` contains
    /// `<`/`.`/`>`, all outside the wire charset, so it can never be
    /// transmitted regardless of report-bearing status.
    #[tokio::test]
    async fn genuinely_unresolvable_caller_hash_retires_fast_not_a_hang() {
        let mut config = test_config();
        config.our_callsign = "YS/WE9G".to_string(); // compound operator
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);
        let mut events = manager.subscribe();

        let qso_id = manager.start_cq(14074000.0, None, false).await.unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        // A standard-callsign station replies to our compound CQ, but we
        // (this decoder, this session) have NEVER heard their plaintext
        // callsign in any standard-format frame before -- their hash
        // genuinely cannot resolve. This is exactly what
        // `MessageExchange::parse_message`/`normalize_callsign_token`
        // hands the QSO engine for that case: the literal placeholder,
        // left untouched (there is no real callsign to normalize it to).
        let parsed = MessageType::CqResponse {
            calling_station: "YS/WE9G".to_string(),
            responding_station: "<...>".to_string(),
            grid: Some("EM10".to_string()),
        };

        manager
            .process_message(
                parsed,
                "YS/WE9G <...> EM10".to_string(),
                14074000.0,
                Some(-10.0),
            )
            .await
            .unwrap();

        // The QSO advances and latches the unresolved placeholder -- relevance
        // routing for a not-yet-partnered CallingCq QSO only verifies the
        // response is addressed to us, not who the sender claims to be.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(&progress.state, QsoState::WaitingForReport { their_callsign, .. } if their_callsign == "<...>"),
            "expected their_callsign latched as the unresolved placeholder, got: {:?}",
            progress.state
        );

        // The very next watchdog pass retires it -- bounded, fast, not a
        // hang and not a full 5-minute silent loop.
        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));

        let mut reason = None;
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::QsoFailed {
                qso_id: id,
                reason: r,
                ..
            } = ev
            {
                if id == qso_id {
                    reason = Some(r);
                }
            }
        }
        assert!(
            matches!(reason, Some(QsoFailureReason::MessageUnencodable(_))),
            "expected a fast, distinct MessageUnencodable retirement, got {:?}",
            reason
        );
    }

    /// PAN-17 round 3 (Codex re-review of #248, finding 1): a QSO that
    /// reaches a report-bearing rung (here, `SendingReport`) against a
    /// still-compound partner must retire immediately with
    /// `MessageUnencodable`, instead of re-arming an encode that
    /// `try_encode_nonstandard` (pancetta-ft8) will always refuse — the
    /// original PAN-17 symptom relocated to the report stage. `YS/WE9G`
    /// alone passes `callsign_is_wire_representable` (it fits the 58-bit
    /// hash field fine), so this specifically exercises the NEW
    /// report-stage check, not the pre-existing wire-representability one.
    #[tokio::test]
    async fn report_stage_watchdog_retires_compound_partner() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        {
            let mut qsos = manager.qsos.write().await;
            let progress = qsos.get_mut(&qso_id).unwrap();
            progress.state = QsoState::SendingReport {
                their_callsign: "YS/WE9G".to_string(),
                their_report: Some(-10),
                our_report: -10,
                frequency: 14074000.0,
                started_at: start,
            };
        }

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "a report-bearing rung against a compound-callsign partner must retire fast"
        );
    }

    /// The report-stage watchdog must NOT trip for a perfectly ordinary
    /// standard-callsign partner at the same rung -- no false positives.
    #[tokio::test]
    async fn report_stage_watchdog_does_not_retire_standard_partner() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        {
            let mut qsos = manager.qsos.write().await;
            let progress = qsos.get_mut(&qso_id).unwrap();
            progress.state = QsoState::SendingReport {
                their_callsign: "K1DEF".to_string(),
                their_report: Some(-10),
                our_report: -10,
                frequency: 14074000.0,
                started_at: start,
            };
        }

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "a report-bearing rung against a standard-callsign partner must NOT retire early"
        );
    }

    /// A bare `/P` or `/R` suffix is genuinely pack28-representable (the
    /// ONLY two suffixes `pack28` special-cases) -- the report-stage
    /// watchdog must not treat it as unencodable. Regression guard for
    /// `adversarial_compound_calls.rs::base_first_then_portable_suffix_completes`.
    #[tokio::test]
    async fn report_stage_watchdog_does_not_retire_portable_suffix_partner() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("G8BCG/P".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        {
            let mut qsos = manager.qsos.write().await;
            let progress = qsos.get_mut(&qso_id).unwrap();
            progress.state = QsoState::SendingReport {
                their_callsign: "G8BCG/P".to_string(),
                their_report: Some(-5),
                our_report: -5,
                frequency: 14074000.0,
                started_at: start,
            };
        }

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "a /P-suffix partner is pack28-representable and must NOT retire early"
        );
    }

    /// PAN-27 finding 3 (round 4 review): the skip-rung path
    /// `RespondingToCq + ReportAck → WaitingForConfirmation` (a compound-
    /// call DX sends their R-report directly, skipping the plain-report
    /// rung) leaves `their_callsign` compound-shaped in a
    /// `WaitingForConfirmation` state — but the message that state actually
    /// has queued is `FinalConfirmation` (RR73), always representable via
    /// i3=4 regardless of callsign shape. The report-stage watchdog must
    /// NOT retire this QSO; it was about to complete cleanly.
    #[tokio::test]
    async fn report_stage_watchdog_does_not_retire_compound_partner_waiting_for_confirmation() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000;
        config.timeouts.manual_call_watchdog_minutes = 60;
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        {
            let mut qsos = manager.qsos.write().await;
            let progress = qsos.get_mut(&qso_id).unwrap();
            progress.state = QsoState::WaitingForConfirmation {
                their_callsign: "YS/WE9G".to_string(),
                their_report: -10,
                our_report: -10,
                frequency: 14074000.0,
                grid_square: None,
                started_at: start,
            };
        }

        manager
            .check_timeouts_at(start + Duration::seconds(1))
            .await;

        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "WaitingForConfirmation against a compound partner has RR73 (not a \
             numeric report) queued and must NOT retire early"
        );
    }

    #[tokio::test]
    async fn engage_contest_profile_stamps_contest_info() {
        let config = test_config();
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
            .await
            .unwrap();

        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "us-state-qso-party")
            .unwrap();
        manager
            .engage_contest_profile(qso_id, profile)
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        let contest_info = progress
            .metadata
            .contest_info
            .expect("contest_info must be set after engaging a profile");
        assert_eq!(contest_info.contest_name, "us-state-qso-party");
    }

    #[tokio::test]
    async fn engage_contest_profile_errors_for_unknown_qso() {
        let config = test_config();
        let manager = QsoManager::new(config);
        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .next()
            .unwrap();
        let bogus_id = QsoId::new_v4();
        let result = manager.engage_contest_profile(bogus_id, profile).await;
        assert!(matches!(
            result,
            Err(QsoManagerError::QsoNotFound { qso_id }) if qso_id == bogus_id
        ));
    }

    #[tokio::test]
    async fn r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged() {
        // `test_config()`'s default `our_callsign` is "W1ABC", but this test's
        // decode text replays the real PAN-49 incident verbatim ("K5ARH K5TD
        // R EM40" — K5ARH is the operator's real callsign). Override so the
        // message is actually addressed to "us"; otherwise
        // `MessageType::is_addressed_to`'s routing check silently drops the
        // frame before it ever reaches a QSO, independent of the transition
        // arm under test.
        let mut config = test_config();
        config.our_callsign = "K5ARH".to_string();
        let manager = QsoManager::new(config);
        let mut rx = manager.subscribe();

        // No QSO engaged yet — an R+grid ack for an unrelated station must stay
        // NonStandard (today's behavior, unchanged) and route nowhere.
        manager
            .process_message(
                MessageType::NonStandard {
                    text: "K5ARH K5TD R EM40".to_string(),
                },
                "K5ARH K5TD R EM40".to_string(),
                1203.0,
                Some(-11.0),
            )
            .await
            .unwrap();
        assert!(
            drain(&mut rx).is_empty(),
            "an unengaged QSO must not react to an R+grid ack"
        );

        // Engage a QSO with K5TD, then the same text must reclassify and route.
        let qso_id = manager
            .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
            .await
            .unwrap();
        drain(&mut rx); // discard the initial call's MessageToSend
        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "us-state-qso-party")
            .unwrap();
        manager
            .engage_contest_profile(qso_id, profile)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::NonStandard {
                    text: "K5ARH K5TD R EM40".to_string(),
                },
                "K5ARH K5TD R EM40".to_string(),
                1203.0,
                Some(-11.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "engaged QSO must advance on the R+grid ack, got {:?}",
            progress.state
        );
    }

    /// PAN-49 regression: replays the real decode from
    /// ~/.pancetta/logs/pancetta.log.2026-08-30 (K5TD acking our grid during
    /// the 2026-08-29/30 Kansas QSO Party session) that stalled a live manual
    /// QSO before this fix. Must now advance to WaitingForConfirmation with
    /// the grid latched, instead of silently landing in NonStandard forever.
    ///
    /// `our_callsign` is overridden to the operator's real callsign (K5ARH,
    /// not `test_config()`'s default "W1ABC") so the replayed text is
    /// genuinely addressed to "us" — see the comment on
    /// `r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged` above for
    /// why that matters.
    #[tokio::test]
    async fn pan_49_k5td_r_grid_ack_advances_the_qso() {
        let mut config = test_config();
        config.our_callsign = "K5ARH".to_string();
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
            .await
            .unwrap();
        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "us-state-qso-party")
            .unwrap();
        manager
            .engage_contest_profile(qso_id, profile)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::NonStandard {
                    text: "K5ARH K5TD R EM40".to_string(),
                },
                "K5ARH K5TD R EM40".to_string(),
                1203.1,
                Some(-11.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { grid_square: Some(ref g), .. } if g == "EM40"),
            "expected WaitingForConfirmation with grid EM40, got {:?}",
            progress.state
        );
        assert_eq!(progress.metadata.grids.theirs, Some("EM40".to_string()));
        assert_eq!(
            progress
                .metadata
                .contest_info
                .as_ref()
                .map(|c| c.contest_name.as_str()),
            Some("us-state-qso-party")
        );
    }

    /// Final-review Finding 1/3 regression: a `ContestReply` frame that
    /// legitimately reclassifies (because ITS sender — W0D — has its own
    /// contest-engaged QSO, satisfying Finding 2's per-QSO gate) is still
    /// broadcast-routed to every active QSO addressed to us, including an
    /// UNRELATED engaged QSO (K5TD) whose partner is not W0D. That QSO's
    /// transition arm must `reject_sender` (state does not advance), and —
    /// this is the assertion that would have caught Finding 1 before this
    /// fix — the grid-latch block must NOT write W0D's grid into K5TD's
    /// `metadata.grids.theirs` just because the frame reached
    /// `process_message_for_qso` for that QSO.
    #[tokio::test]
    async fn spurious_contest_reply_from_unrelated_station_does_not_advance_or_leak_grid() {
        let mut config = test_config();
        config.our_callsign = "K5ARH".to_string();
        let manager = QsoManager::new(config);
        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "us-state-qso-party")
            .unwrap();

        // QSO under test: engaged with K5TD, RespondingToCq.
        let k5td_qso_id = manager
            .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
            .await
            .unwrap();
        manager
            .engage_contest_profile(k5td_qso_id, profile.clone())
            .await
            .unwrap();

        // A second, unrelated engaged QSO with W0D at the same frequency —
        // its presence is what legitimately unlocks reclassification (per
        // Finding 2's tightened per-QSO gate) for a frame FROM W0D, even
        // though that frame has nothing to do with the K5TD QSO.
        let w0d_qso_id = manager
            .respond_to_cq_manual("W0D".to_string(), 1203.0, None)
            .await
            .unwrap();
        manager
            .engage_contest_profile(w0d_qso_id, profile)
            .await
            .unwrap();

        // W0D acks OUR grid — legitimate traffic for the W0D QSO, but must
        // not be able to advance or contaminate the unrelated K5TD QSO.
        manager
            .process_message(
                MessageType::NonStandard {
                    text: "K5ARH W0D R EM28".to_string(),
                },
                "K5ARH W0D R EM28".to_string(),
                1203.0,
                Some(-11.0),
            )
            .await
            .unwrap();

        let k5td_progress = manager.get_qso(k5td_qso_id).await.unwrap();
        assert!(
            matches!(k5td_progress.state, QsoState::RespondingToCq { .. }),
            "K5TD QSO must not advance on a ContestReply from an unrelated sender, got {:?}",
            k5td_progress.state
        );
        assert_eq!(
            k5td_progress.metadata.grids.theirs, None,
            "W0D's grid must not leak into the unrelated K5TD QSO's metadata"
        );
    }

    /// PR #344 round-1 Codex P2 regression: PAN-51's `ft8_message_to_qso_type`
    /// classifies a native ReplyWithR decode directly as `ContestReply`,
    /// never routing through `process_message_with_parity`'s NonStandard
    /// reclassification step -- so a QSO that never called
    /// `engage_contest_profile` must still decline to advance (and must not
    /// latch the grid) when it receives a natively-typed `ContestReply` from
    /// its own real partner, exactly matching the pre-PAN-51 (no-contest)
    /// behavior for a decode ft8_lib recognized but pancetta never engaged a
    /// contest profile for.
    #[tokio::test]
    async fn native_contest_reply_ignored_when_qso_not_contest_engaged() {
        let mut config = test_config();
        config.our_callsign = "K5ARH".to_string();
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
            .await
            .unwrap();
        // Deliberately never calls engage_contest_profile.

        manager
            .process_message(
                MessageType::ContestReply {
                    to_station: "K5ARH".to_string(),
                    from_station: "K9ZZ".to_string(),
                    grid: "EN37".to_string(),
                    is_ack: true,
                },
                "K5ARH K9ZZ R EN37".to_string(),
                1500.0,
                Some(-11.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "non-contest-engaged QSO must not advance on a native ContestReply, got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.grids.theirs, None,
            "grid must not latch for a non-contest-engaged QSO's native ContestReply"
        );
    }

    /// Companion to the above: the SAME frame DOES advance the QSO once it
    /// is contest-engaged -- proving the new gate blocks only the
    /// unengaged case, not `ContestReply` handling generally.
    #[tokio::test]
    async fn native_contest_reply_advances_when_qso_contest_engaged() {
        let mut config = test_config();
        config.our_callsign = "K5ARH".to_string();
        let manager = QsoManager::new(config);
        let profile = crate::contest::catalog::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "us-state-qso-party")
            .unwrap();

        let qso_id = manager
            .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
            .await
            .unwrap();
        manager
            .engage_contest_profile(qso_id, profile)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::ContestReply {
                    to_station: "K5ARH".to_string(),
                    from_station: "K9ZZ".to_string(),
                    grid: "EN37".to_string(),
                    is_ack: true,
                },
                "K5ARH K9ZZ R EN37".to_string(),
                1500.0,
                Some(-11.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "contest-engaged QSO must advance on a native ContestReply, got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.grids.theirs,
            Some("EN37".to_string()),
            "grid must latch for a contest-engaged QSO's native ContestReply"
        );
    }

    /// Repetitive-TX watchdog: a QSO stuck in the same active TX state (we keep
    /// re-sending the same message) is retired at repetitive_tx_timeout_secs,
    /// independent of (and tighter than) the manual keep-call watchdog.
    #[tokio::test]
    async fn repetitive_tx_watchdog_retires_stuck_state() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000; // non-binding
        config.timeouts.manual_call_watchdog_minutes = 60; // non-binding (>>2min)
        config.timeouts.repetitive_tx_timeout_secs = 120;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

        // Just under 2 min in the same state: still alive.
        manager
            .check_timeouts_at(start + Duration::seconds(119))
            .await;
        assert!(manager.get_qso(qso_id).await.is_ok());

        // Past 2 min stuck re-sending the same message: retired by the cap
        // (manual watchdog bounds are deliberately non-binding here).
        manager
            .check_timeouts_at(start + Duration::seconds(121))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));
    }

    /// rearm_manual_calls re-emits a CqResponse MessageToSend and increments
    /// the call count — but only once per slot, and not for auto QSOs.
    #[tokio::test]
    async fn rearm_emits_call_once_per_slot_for_manual_only() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let manual_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let _auto_id = manager
            .respond_to_cq("K9ZZ".to_string(), 14076000.0, None)
            .await
            .unwrap();

        // Drain the two initial MessageToSend events from the responses.
        let mut initial = 0;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, QsoEvent::MessageToSend { .. }) {
                initial += 1;
            }
        }
        assert_eq!(initial, 2, "two initial calls (one manual, one auto)");

        // Re-arm too soon (same instant as start): no new call.
        let start = manager
            .get_qso(manual_id)
            .await
            .unwrap()
            .metadata
            .start_time;
        manager.rearm_manual_calls_at(start).await;
        let mut too_soon = 0;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, QsoEvent::MessageToSend { .. }) {
                too_soon += 1;
            }
        }
        assert_eq!(too_soon, 0, "re-arm within a slot must not re-call");

        // Re-arm a slot later: exactly one new call, for the manual QSO.
        manager
            .rearm_manual_calls_at(start + Duration::seconds(15))
            .await;
        let mut recalls = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { qso_id, .. } = ev {
                recalls.push(qso_id);
            }
        }
        assert_eq!(recalls.len(), 1, "exactly one re-call");
        assert_eq!(recalls[0], manual_id, "only the manual QSO is re-called");
        assert_eq!(
            manager
                .get_qso(manual_id)
                .await
                .unwrap()
                .metadata
                .call_count,
            2
        );
    }

    /// resend_last_tx on a QSO with a prior Sent message re-emits a
    /// MessageToSend carrying that message.
    #[tokio::test]
    async fn resend_last_tx_reemits_last_sent() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Drain the initial call event.
        while events.try_recv().is_ok() {}

        manager.resend_last_tx(qso_id).await.unwrap();

        // Expect exactly one MessageToSend, re-emitting the initial CqResponse.
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend {
                qso_id, message, ..
            } = ev
            {
                resends.push((qso_id, message));
            }
        }
        assert_eq!(resends.len(), 1, "exactly one resend event");
        assert_eq!(resends[0].0, qso_id);
        assert!(matches!(resends[0].1, MessageType::CqResponse { .. }));
    }

    /// resend_last_tx stamps `last_call_at` and bumps `call_count` exactly
    /// like the keep-call rearm does, so an operator-triggered resend is
    /// accounted for watchdog/cap purposes and the rearm/coalescer
    /// machinery is aware of it (double-PTT hardening,
    /// docs/qso-tx-deep-review-2026-07-18.md).
    #[tokio::test]
    async fn resend_last_tx_stamps_last_call_at_and_call_count() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        while events.try_recv().is_ok() {}

        let before = manager.get_qso(qso_id).await.unwrap().metadata;
        assert_eq!(before.call_count, 1);
        let last_call_before = before.last_call_at;

        // Ensure the clock actually advances so last_call_at is observably
        // different, mirroring the rearm test's own timing discipline.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        manager.resend_last_tx(qso_id).await.unwrap();

        let after = manager.get_qso(qso_id).await.unwrap().metadata;
        assert_eq!(
            after.call_count, 2,
            "resend must bump call_count exactly like a rearm re-send"
        );
        assert!(
            after.last_call_at.is_some() && after.last_call_at != last_call_before,
            "resend must stamp last_call_at to now, like rearm_manual_calls_at does"
        );
    }

    /// resend_last_tx on an unknown QSO id returns QsoNotFound.
    #[tokio::test]
    async fn resend_last_tx_unknown_id_not_found() {
        let manager = QsoManager::new(test_config());
        let bogus = QsoId::new_v4();
        let err = manager.resend_last_tx(bogus).await.unwrap_err();
        assert!(matches!(err, QsoManagerError::QsoNotFound { .. }));
    }

    /// apply_tx_offset_switch on a QSO updates frequency and resets stall_cycles.
    #[tokio::test]
    async fn apply_tx_offset_switch_updates_frequency_and_resets_stall_cycles() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Bump stall_cycles by directly manipulating the QSO's metadata.
        {
            let mut qsos = manager.qsos.write().await;
            if let Some(qso) = qsos.get_mut(&qso_id) {
                qso.metadata.stall_cycles = 5;
            }
        }

        // Verify stall_cycles was bumped.
        let before = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(before.metadata.stall_cycles, 5);
        assert_ne!(before.metadata.frequency, 1800.0);

        // Apply the offset switch.
        manager
            .apply_tx_offset_switch(qso_id, 1800.0, None)
            .await
            .unwrap();

        // Verify frequency was updated and stall_cycles was reset.
        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after.metadata.frequency, 1800.0);
        assert_eq!(after.metadata.stall_cycles, 0);
    }

    /// PAN-72 final review (finding 4): `apply_tx_offset_switch` must also
    /// update the QSO **state**'s own embedded frequency, exactly as the
    /// Hound QSY block hand-writes `SendingReport.frequency` for the same
    /// reason: later transitions (notably `Completed`) are built from the
    /// preceding state's `frequency`, so updating only `metadata.frequency`
    /// would log the pre-switch offset.
    #[tokio::test]
    async fn apply_tx_offset_switch_updates_the_state_embedded_frequency_too() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        let before = manager.get_qso(qso_id).await.unwrap();
        assert_ne!(
            before.state.frequency(),
            Some(1800.0),
            "precondition: the state is not already on the target offset"
        );

        manager
            .apply_tx_offset_switch(qso_id, 1800.0, None)
            .await
            .unwrap();

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.state.frequency(),
            Some(1800.0),
            "the state's embedded frequency must follow metadata.frequency, or \
             the eventual Completed record logs the stale offset"
        );
        assert_eq!(after.metadata.frequency, 1800.0);
    }

    /// PAN-72 final review (finding 6.4): the offset is clamped defensively,
    /// since this is a `pub` method documented as the sole external mutator of
    /// an active QSO's TX offset. The applied (clamped) value is returned so
    /// the coordinator can mirror it into `active_tx_offsets`.
    #[tokio::test]
    async fn apply_tx_offset_switch_clamps_out_of_band_offsets() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        let applied = manager
            .apply_tx_offset_switch(qso_id, 9000.0, None)
            .await
            .unwrap();
        assert_eq!(applied, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after.metadata.frequency, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
        assert_eq!(after.state.frequency(), Some(ACTIVE_QSO_TX_OFFSET_MAX_HZ));

        let applied = manager
            .apply_tx_offset_switch(qso_id, -50.0, None)
            .await
            .unwrap();
        assert_eq!(applied, ACTIVE_QSO_TX_OFFSET_MIN_HZ);
        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after.metadata.frequency, ACTIVE_QSO_TX_OFFSET_MIN_HZ);
        assert_eq!(after.state.frequency(), Some(ACTIVE_QSO_TX_OFFSET_MIN_HZ));

        // In-band values pass through untouched.
        let applied = manager
            .apply_tx_offset_switch(qso_id, 1234.0, None)
            .await
            .unwrap();
        assert_eq!(applied, 1234.0);
    }

    /// The clamp must NOT be the narrower autonomous *pick* band
    /// (`TX_OFFSET_MIN_HZ`..=`TX_OFFSET_MAX_HZ`, 300–2700). Answering a CQ
    /// ties our reply to wherever we decoded the DX, unclamped, up to
    /// ~2900 Hz — so a `Revert`'s `target_hz` (a `last_known_good_offset_hz`
    /// that was itself such a reply offset) must survive intact. Narrowing it
    /// would move the QSO off the very offset it demonstrably worked on,
    /// defeating the point of reverting.
    #[tokio::test]
    async fn apply_tx_offset_switch_preserves_a_legitimate_high_reply_offset() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // 2850 Hz: above the autonomous pick band's 2700 ceiling, but a
        // perfectly legitimate place for an in-progress QSO to be sitting.
        let applied = manager
            .apply_tx_offset_switch(qso_id, 2850.0, None)
            .await
            .unwrap();
        assert_eq!(
            applied, 2850.0,
            "a revert to a real, previously-working high offset must not be \
             narrowed to the autonomous pick band's ceiling"
        );
        assert!(applied > TX_OFFSET_MAX_HZ);

        // Likewise at the low end (the pick band starts at 300).
        let applied = manager
            .apply_tx_offset_switch(qso_id, 250.0, None)
            .await
            .unwrap();
        assert_eq!(applied, 250.0);
        assert!(applied < TX_OFFSET_MIN_HZ);
    }

    /// apply_tx_offset_switch on an unknown QSO id returns QsoNotFound.
    #[tokio::test]
    async fn apply_tx_offset_switch_on_unknown_qso_returns_not_found() {
        let manager = QsoManager::new(test_config());
        let result = manager
            .apply_tx_offset_switch(QsoId::new_v4(), 1800.0, None)
            .await;
        assert!(matches!(result, Err(QsoManagerError::QsoNotFound { .. })));
    }

    /// FIX 4: a manual QSO in SendingReport (we sent R, DX has not advanced)
    /// re-emits our R-report (ReportAck) each slot when re-armed.
    #[tokio::test]
    async fn rearm_resends_r_report_in_sending_report() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        // Advance to SendingReport: DX sends us a report; we send R-report.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -7,
                },
                "W1ABC K1DEF -07".to_string(),
                14074000.0,
                Some(-12.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        // Drain the initial call + the auto-sequenced R-report.
        while events.try_recv().is_ok() {}

        // A slot later, with the DX still not advancing, re-arm re-sends our
        // R-report (ReportAck), not a fresh call.
        let last = progress.metadata.last_call_at.unwrap();
        manager
            .rearm_manual_calls_at(last + Duration::seconds(15))
            .await;
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                resends.push(message);
            }
        }
        assert_eq!(resends.len(), 1, "exactly one re-send, got {:?}", resends);
        match &resends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, "K1DEF");
                assert_eq!(from_station, "W1ABC");
                assert_eq!(*report, -12, "re-sends our latched R-report");
            }
            other => panic!("expected ReportAck re-send, got {:?}", other),
        }
    }

    /// The KJ5NJF bug (docs/qso-engine-bugs.md BUG 2 / qso-state-machine-
    /// analysis.md GAP-2): a caller who entered SendingReport via the
    /// stuck-at-grid arm (DX re-sent grid/call, not a report — so we sent a
    /// PLAIN SignalReport and their_report is still None) must have the rearm
    /// re-send that SAME plain SignalReport, never escalate to a ReportAck
    /// (R-report) we never actually sent. Escalating here previously drove the
    /// QSO's own outbound rung forward with zero input from the DX.
    #[tokio::test]
    async fn rearm_resends_plain_report_when_their_report_is_none() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("KJ5NJF".to_string(), 1650.0, None)
            .await
            .unwrap();
        // The DX re-sends our grid exchange (K5ARH KJ5NJF EM12) — copied us,
        // but has NOT sent a report yet. This is the stuck-at-grid arm:
        // RespondingToCq + CqResponse -> SendingReport{their_report: None}.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "KJ5NJF".to_string(),
                    grid: Some("EM12".to_string()),
                },
                "W1ABC KJ5NJF EM12".to_string(),
                1650.0,
                Some(-14.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        match &progress.state {
            QsoState::SendingReport { their_report, .. } => {
                assert!(
                    their_report.is_none(),
                    "stuck-at-grid entry must carry their_report=None"
                );
            }
            other => panic!("expected SendingReport, got {:?}", other),
        }
        while events.try_recv().is_ok() {}

        // A slot later, with the DX STILL not having sent a report, the rearm
        // must re-send our plain -NN report, NOT escalate to R-NN.
        let last = progress.metadata.last_call_at.unwrap();
        manager
            .rearm_manual_calls_at(last + Duration::seconds(15))
            .await;
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                resends.push(message);
            }
        }
        assert_eq!(resends.len(), 1, "exactly one re-send, got {:?}", resends);
        match &resends[0] {
            MessageType::SignalReport {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, "KJ5NJF");
                assert_eq!(from_station, "W1ABC");
                assert_eq!(*report, -14, "re-sends our latched (unacked) report");
            }
            other => panic!(
                "expected a plain SignalReport re-send (NOT ReportAck — the KJ5NJF bug), got {:?}",
                other
            ),
        }
    }

    /// Rearm-coordination fix (qso-state-machine-analysis.md Symptom A): a
    /// forward state advance stamps `last_call_at`, so the per-slot rearm does
    /// NOT also fire (with the stale/old rung) in the same slot the DX's
    /// response just advanced us. Without this, a late-decoding response and
    /// the wall-clock rearm could both queue a TransmitRequest for one slot.
    #[tokio::test]
    async fn forward_advance_stamps_last_call_at_suppressing_same_slot_rearm() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        while events.try_recv().is_ok() {}

        // The DX's report arrives 12s into the slot (a realistically late
        // decode — well under the 15s SLOT_SECONDS rearm threshold measured
        // from `opened_at`, but this IS a genuine forward advance).
        let response_at = opened_at + Duration::seconds(12);
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -7,
                },
                "W1ABC K1DEF -07".to_string(),
                14074000.0,
                Some(-12.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        // The forward advance emits its own auto-sequenced reply (R-report) —
        // drain it so only the REARM's output (if any) remains below.
        while events.try_recv().is_ok() {}

        // Call the rearm at the ORIGINAL slot boundary (opened_at + 15s) --
        // before this fix, last_call_at was untouched by the advance, so this
        // would still see elapsed_since_last >= 15s from opened_at and
        // re-emit a stale rung in the very slot the DX just answered.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                resends.push(message);
            }
        }
        assert!(
            resends.is_empty(),
            "the rearm must be suppressed the same slot a forward advance occurred, got {:?}",
            resends
        );

        // Sanity: the rearm is NOT permanently disabled — one full slot after
        // the ADVANCE (not the original open), it does fire again.
        manager
            .rearm_manual_calls_at(response_at + Duration::seconds(15))
            .await;
        let mut resends2 = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                resends2.push(message);
            }
        }
        assert_eq!(
            resends2.len(),
            1,
            "rearm must resume one slot after the advance, got {:?}",
            resends2
        );
    }

    /// qso-state-machine-analysis GAP-1: a DX that copies us on the first
    /// exchange and closes straight from our opening grid (RespondingToCq)
    /// with RR73 must complete the QSO (previously stalled at grid forever).
    #[tokio::test]
    async fn responding_to_cq_early_close_with_rr73_completes() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        while events.try_recv().is_ok() {}

        manager
            .process_message(
                MessageType::FinalConfirmation {
                    from_station: "K1DEF".to_string(),
                    to_station: "W1ABC".to_string(),
                },
                "W1ABC K1DEF RR73".to_string(),
                14074000.0,
                Some(-9.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );

        // The auto-sequenced reply must be our closing 73.
        let mut sent = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                sent.push(message);
            }
        }
        assert_eq!(sent.len(), 1, "expected exactly one reply, got {:?}", sent);
        match &sent[0] {
            MessageType::SeventyThree {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, "K1DEF");
                assert_eq!(from_station, "W1ABC");
            }
            other => panic!("expected SeventyThree reply, got {:?}", other),
        }
    }

    /// GAP-1: a plain "73" close (not RR73) from RespondingToCq also
    /// completes, and — matching the SendingReport+73 arm — we do NOT
    /// re-send our own 73 (they're already done; re-sending only adds QRM).
    #[tokio::test]
    async fn responding_to_cq_early_close_with_plain_73_completes_no_resend() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        while events.try_recv().is_ok() {}

        manager
            .process_message(
                MessageType::SeventyThree {
                    from_station: "K1DEF".to_string(),
                    to_station: "W1ABC".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                14074000.0,
                Some(-9.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::Completed { .. }));

        let mut sent = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                sent.push(message);
            }
        }
        assert!(
            sent.is_empty(),
            "must NOT re-send a 73 once the DX already said 73, got {:?}",
            sent
        );
    }

    /// GAP-1 sender verification: a spoofed RR73 (wrong from_station) must
    /// NOT complete the QSO — every other arm is sender-verified and this new
    /// one must be too.
    #[tokio::test]
    async fn responding_to_cq_early_close_rejects_spoofed_sender() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::FinalConfirmation {
                    from_station: "W9ZZZ".to_string(), // NOT the QSO partner
                    to_station: "W1ABC".to_string(),
                },
                "W1ABC W9ZZZ RR73".to_string(),
                14074000.0,
                Some(-9.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a spoofed RR73 must not advance the QSO, got {:?}",
            progress.state
        );
    }

    /// FIX 4: the watchdog still retires a SendingReport manual QSO that
    /// never advances — re-sending our R-report cannot loop forever.
    #[tokio::test]
    async fn watchdog_retires_stalled_sending_report() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000; // not the binding bound
        config.timeouts.manual_call_watchdog_minutes = 5;
        // Isolate the 5-min manual watchdog from the (tighter) repetitive-TX cap.
        config.timeouts.repetitive_tx_timeout_secs = 100_000;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -7,
                },
                "W1ABC K1DEF -07".to_string(),
                14074000.0,
                Some(-12.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));

        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
        // Just under the watchdog: still alive.
        manager
            .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
            .await;
        assert!(manager.get_qso(qso_id).await.is_ok());
        // Past the watchdog: retired.
        manager
            .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));
    }

    // --- Batch 2 #6: CQer (we-CQed) completion path -----------------------

    /// A full we-CQed exchange must advance all the way to Completed and emit
    /// QsoCompleted (it previously stalled in WaitingForReport — no arm out).
    /// We drive it via `process_message`, feeding the caller's messages, and
    /// assert the QSO completes and logs the caller's grid (Batch 2 #8).
    #[tokio::test]
    async fn cqer_full_sequence_completes_and_logs_grid() {
        // our_callsign = W1ABC (from test_config).
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq(freq, None, false).await.unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::CallingCq { .. }
        ));
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.role,
            QsoRole::Cqer
        );

        // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::WaitingForReport { .. }),
            "CallingCq + CqResponse → WaitingForReport, got {:?}",
            p.state
        );

        // Caller rogers our report with their R-report: "W1ABC K1DEF R-12".
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::WaitingForConfirmation { .. }),
            "WaitingForReport + ReportAck → WaitingForConfirmation, got {:?}",
            p.state
        );

        // Caller closes with 73: "W1ABC K1DEF 73" → Completed.
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "WaitingForConfirmation + 73 → Completed, got {:?}",
            p.state
        );
        // Batch 2 #8: the caller's grid latched from the opening CqResponse is
        // carried into the logged metadata.
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
        assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
        assert!(p.metadata.end_time.is_some());
    }

    /// PAN-14: a single station must never end up as the partner of TWO
    /// simultaneously-active QSO objects — that produces two independent
    /// TX cadences (two different `qso_id`s, potentially two different
    /// frequency offsets) for the SAME real-world station, which is exactly
    /// the "two very-high-amplitude signals for one station in one TX
    /// window" symptom reported on-air 2026-08-11.
    ///
    /// Reproduction (mirrors deep-review finding SM-F7,
    /// docs/qso-tx-deep-review-2026-07-18.md §A.3): `find_qsos_for_message`
    /// routes a message to EVERY active QSO whose `classify_relevance` arm
    /// matches, and `process_message_with_parity` then advances ALL of
    /// them independently. A `CallingCq` QSO's routing arms accept a
    /// SignalReport/CqResponse from ANY station addressed to us (correct —
    /// we don't yet know who will answer our CQ). But if we ALSO already
    /// have a separate, already-partnered QSO with that exact station
    /// (e.g. we just called their CQ and are `RespondingToCq`, waiting for
    /// their report), and that station's report happens to land within the
    /// CallingCq QSO's frequency gate, the SAME decoded frame satisfies
    /// BOTH QSOs' relevance arms. Unlike the create-time paths
    /// (`respond_to_cq_with`/`respond_to_caller`), this message-routing
    /// path never calls `supersede_active_qsos_for` — nothing in
    /// `find_qsos_for_message` excludes a station that already has an
    /// active partner elsewhere. Both QSOs advance, and now K1DEF is the
    /// active partner of two different `qso_id`s.
    #[tokio::test]
    async fn calling_cq_and_established_qso_both_accept_same_partners_frame() {
        let manager = QsoManager::new(test_config());

        // QSO A: we already called K1DEF's CQ (e.g. a manual/auto answer)
        // and are RespondingToCq, waiting for K1DEF's own report of us.
        // K1DEF's real TX frequency is 1500.0 Hz.
        let qso_a = manager
            .respond_to_cq_with(
                "K1DEF".to_string(),
                1500.0,
                None,
                CallInitiation::Manual,
                None,
                false,
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_a).await.unwrap().state,
            QsoState::RespondingToCq { .. }
        ));

        // QSO B: completely independent — we are also running our own CQ
        // loop, TXing at 1505.0 Hz (5 Hz from K1DEF's real frequency — well
        // within the CallingCq arm's 15 Hz gate; a routine coincidence on a
        // busy band, not an attacker-crafted collision).
        let qso_b = manager.start_cq(1505.0, None, false).await.unwrap();
        assert!(matches!(
            manager.get_qso(qso_b).await.unwrap().state,
            QsoState::CallingCq { .. }
        ));

        // K1DEF sends their report of us, addressed to us, decoded at their
        // real 1500.0 Hz frequency: "W1ABC K1DEF -09". This is EXACTLY the
        // frame QSO A is waiting for (RespondingToCq + SignalReport{from
        // K1DEF, to us}). It is ALSO, independently, a bare-report answer
        // to QSO B's CQ (CallingCq + SignalReport{to us} — any from_station
        // qualifies, since a CallingCq QSO doesn't know who will answer).
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -9,
                },
                "W1ABC K1DEF -09".to_string(),
                1500.0,
                Some(-9.0),
            )
            .await
            .unwrap();

        let active = manager.get_active_qsos().await;
        let k1def_partners: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .collect();

        // BUG (if reproduced): both qso_a and qso_b are active AND both are
        // now partnered with K1DEF — two independent qso_ids that will each
        // generate their own TX cadence for the same real station. The
        // invariant is exactly one active QSO per (callsign, band) —
        // AGENTS.md, "At most one QSO object exists per (callsign, band)".
        assert_eq!(
            k1def_partners.len(),
            1,
            "K1DEF must be the active partner of exactly ONE QSO object, not {}: {:#?}",
            k1def_partners.len(),
            k1def_partners
                .iter()
                .map(|(id, p)| (*id, p.state.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// PAN-14 P1 (Codex round-1 review of PR #250): TWO unpartnered
    /// `CallingCq` QSOs can coexist — nothing supersedes an existing
    /// `CallingCq` QSO when `start_cq`/`start_cq_manual` opens another one
    /// (repeated manual `c` presses, or Fox mode engaging while a CQ is
    /// already live). Both have `metadata.their_callsign == None`, so the
    /// `sender_has_other_active_or_recent_partner` guard — keyed on an
    /// ESTABLISHED partner — cannot see a conflict between them: a single
    /// incoming reply independently satisfies both QSOs' "any station"
    /// relevance arms. Without a fix, both would advance and partner the
    /// SAME real station with two different `qso_id`s (the same failure
    /// class PAN-14 exists to close, via a different pair of states).
    #[tokio::test]
    async fn one_reply_can_only_advance_the_earliest_calling_cq_not_multiple() {
        let manager = QsoManager::new(test_config());

        // Two independent, still-unpartnered CallingCq QSOs, 5 Hz apart —
        // both well within the CallingCq arm's 15 Hz gate for the same
        // incoming decode.
        let qso_x = manager.start_cq(1500.0, None, false).await.unwrap();
        // Sleep so `metadata.start_time` orders deterministically (real
        // `Utc::now()` calls back-to-back could otherwise tie).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let qso_y = manager.start_cq(1505.0, None, false).await.unwrap();
        assert!(matches!(
            manager.get_qso(qso_x).await.unwrap().state,
            QsoState::CallingCq { .. }
        ));
        assert!(matches!(
            manager.get_qso(qso_y).await.unwrap().state,
            QsoState::CallingCq { .. }
        ));

        // K1DEF answers with a bare report (grid skipped), addressed to us —
        // the A4 CallingCq routing arm, which accepts ANY from_station.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -9,
                },
                "W1ABC K1DEF -09".to_string(),
                1500.0,
                Some(-9.0),
            )
            .await
            .unwrap();

        let active = manager.get_active_qsos().await;
        let k1def_partners: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .collect();
        assert_eq!(
            k1def_partners.len(),
            1,
            "K1DEF must become the partner of exactly ONE CallingCq QSO, not {}: {:#?}",
            k1def_partners.len(),
            k1def_partners
                .iter()
                .map(|(id, p)| (*id, p.state.clone()))
                .collect::<Vec<_>>()
        );

        // The earliest-created CQ must be the one that claimed it; the
        // later one must remain un-advanced (available for its own future
        // caller).
        let p_x = manager.get_qso(qso_x).await.unwrap();
        assert!(
            matches!(p_x.state, QsoState::WaitingForReport { .. }),
            "earliest CQ (qso_x) should advance, got {:?}",
            p_x.state
        );
        let p_y = manager.get_qso(qso_y).await.unwrap();
        assert!(
            matches!(p_y.state, QsoState::CallingCq { .. }),
            "later CQ (qso_y) must remain un-advanced, got {:?}",
            p_y.state
        );
    }

    /// PAN-14 P2 (Codex round-1 review of PR #250): the routing guard only
    /// checked `p.state.is_active()`, so a QSO that just COMPLETED goes
    /// invisible to it the instant it terminates — a stray/duplicate frame
    /// from that exact station (or a fresh CQ-answer attempt) landing
    /// within an unrelated CallingCq QSO's frequency gate could open a
    /// SECOND active QSO for a station we finished working seconds ago.
    /// Mirrors the existing `has_active_or_recent_qso_with` /
    /// `COMPLETED_QSO_REWORK_GRACE` (45 s) "active-or-recently-completed"
    /// pattern used elsewhere in this file.
    #[tokio::test]
    async fn recently_completed_qso_reserves_the_sender_from_an_unrelated_calling_cq() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;

        // Complete a full CQ exchange with K1DEF (mirrors
        // cqer_full_sequence_completes_and_logs_grid).
        let qso_a = manager.start_cq(freq, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_a).await.unwrap().state,
            QsoState::Completed { .. }
        ));

        // Immediately start a NEW, unrelated CQ 5 Hz away — well within the
        // CallingCq arm's 15 Hz gate for a frame decoded at qso_b's offset.
        let qso_b = manager.start_cq(freq + 5.0, None, false).await.unwrap();

        // K1DEF sends a stray/duplicate CqResponse-shaped frame again,
        // within the completed-QSO grace window (this test runs in
        // milliseconds, well under COMPLETED_QSO_REWORK_GRACE's 45 s).
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq + 5.0,
                Some(-10.0),
            )
            .await
            .unwrap();

        let p_b = manager.get_qso(qso_b).await.unwrap();
        assert!(
            matches!(p_b.state, QsoState::CallingCq { .. }),
            "qso_b must remain un-advanced — K1DEF is reserved by its recent completion, got {:?}",
            p_b.state
        );
    }

    /// PAN-25 (Codex round-2 review of PR #250): the recently-completed
    /// suppression above must be scoped to the completed QSO's band — the
    /// uniqueness invariant (AGENTS.md) is per (callsign, band), not per
    /// callsign alone. A station worked on 20m and answering a fresh CQ on
    /// 40m within the same grace window is a legitimate new QSO and must
    /// advance normally.
    #[tokio::test]
    async fn recently_completed_qso_on_a_different_band_does_not_reserve_the_sender() {
        let manager = QsoManager::new(test_config());
        let freq_20m = 14074000.0;
        let freq_40m = 7074000.0;

        // Complete a full CQ exchange with K1DEF on 20m.
        let qso_a = manager.start_cq(freq_20m, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq_20m,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq_20m,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq_20m,
                Some(-11.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_a).await.unwrap().state,
            QsoState::Completed { .. }
        ));

        // Start a NEW, unrelated CQ on 40m — a different band from the
        // just-completed 20m QSO.
        let qso_b = manager.start_cq(freq_40m + 5.0, None, false).await.unwrap();

        // K1DEF answers on 40m, well within the completed-QSO grace window.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq_40m + 5.0,
                Some(-10.0),
            )
            .await
            .unwrap();

        let p_b = manager.get_qso(qso_b).await.unwrap();
        assert!(
            matches!(p_b.state, QsoState::WaitingForReport { .. }),
            "qso_b must advance — K1DEF's 20m completion must not reserve it on 40m, got {:?}",
            p_b.state
        );
    }

    /// PAN-25 round 1 (Codex): the test above proves the *comparison logic*
    /// but supplies full RF frequencies (14074000.0, 7074000.0) directly as
    /// `frequency` — in the live coordinator path `frequency` is always a
    /// small AUDIO OFFSET (a few hundred/thousand Hz), and pre-this-round the
    /// stored `metadata.frequency` for a Completed QSO was too, so
    /// `frequency_to_band` returned the same "0MHZ" bucket for both
    /// regardless of the real RF band — the fix looked correct but was
    /// inert in production. This test uses realistic small offsets and a
    /// shared dial-frequency source that changes between the two QSOs
    /// (exactly the "operator changes bands" scenario PAN-25 describes),
    /// proving the actual dial-adjustment mechanism the production code
    /// path uses.
    #[tokio::test]
    async fn recently_completed_qso_on_a_different_dial_band_does_not_reserve_the_sender() {
        let mut manager = QsoManager::new(test_config());
        let dial = std::sync::Arc::new(AtomicU64::new(14_074_000)); // 20m
        manager.set_dial_frequency_source(dial.clone());
        let audio_offset = 1500.0; // realistic small in-passband offset

        // Complete a full CQ exchange with K1DEF on 20m (dial 14.074 MHz).
        let qso_a = manager.start_cq(audio_offset, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                audio_offset,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        let completed = manager.get_qso(qso_a).await.unwrap();
        assert!(matches!(completed.state, QsoState::Completed { .. }));
        // PAN-25 round 2: `frequency` must stay the audio offset (other
        // consumers like `resend_last_tx` depend on it) -- the true RF
        // frequency is stamped separately into `completed_rf_frequency_hz`.
        assert_eq!(
            completed.metadata.frequency, audio_offset,
            "completed QSO's stored frequency must remain the audio offset, not be overwritten"
        );
        assert_eq!(
            completed.metadata.completed_rf_frequency_hz,
            Some(14_074_000.0 + audio_offset),
            "completed QSO's true RF frequency must be stamped into completed_rf_frequency_hz"
        );

        // Operator moves to 40m — a real dial change, within the grace window.
        dial.store(7_074_000, Ordering::Relaxed);

        // A NEW, unrelated CQ at the SAME small audio offset (plausible: the
        // new CQ just happens to land in a similar spot in the passband).
        let qso_b = manager
            .start_cq(audio_offset + 5.0, None, false)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                audio_offset + 5.0,
                Some(-10.0),
            )
            .await
            .unwrap();

        let p_b = manager.get_qso(qso_b).await.unwrap();
        assert!(
            matches!(p_b.state, QsoState::WaitingForReport { .. }),
            "qso_b must advance — K1DEF's 20m completion (different dial band) must not \
             reserve it on 40m, got {:?}",
            p_b.state
        );
    }

    /// PAN-25 round 2 (Codex P1): a real regression the earlier version of
    /// this fix introduced — stamping `metadata.frequency` itself with the
    /// RF value made `resend_last_tx` (a close-step retry / 73-recovery)
    /// try to key at ~14 MHz, which the TX worker correctly rejects as
    /// exceeding its ~3100 Hz modulation limit, so the 73 never actually
    /// transmits. Proves `resend_last_tx` on a COMPLETED QSO (with a real
    /// dial configured, so `completed_rf_frequency_hz` is genuinely
    /// populated) still resends at the audio-offset frequency, not the RF
    /// one.
    #[tokio::test]
    async fn resend_last_tx_on_a_completed_qso_uses_the_audio_offset_not_rf_frequency() {
        let mut manager = QsoManager::new(test_config());
        manager.set_dial_frequency_source(std::sync::Arc::new(AtomicU64::new(14_074_000)));
        let audio_offset = 1500.0;

        let qso_id = manager.start_cq(audio_offset, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                audio_offset,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        let completed = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(completed.state, QsoState::Completed { .. }));
        assert!(
            completed.metadata.completed_rf_frequency_hz.is_some(),
            "precondition: this test needs a real dial-adjusted RF frequency stamped, to prove \
             resend_last_tx doesn't accidentally pick it up"
        );

        let mut rx = manager.subscribe();
        manager.resend_last_tx(qso_id).await.unwrap();
        let mut resent_frequency = None;
        while let Ok(event) = rx.try_recv() {
            if let QsoEvent::MessageToSend { frequency, .. } = event {
                resent_frequency = Some(frequency);
            }
        }
        assert_eq!(
            resent_frequency,
            Some(audio_offset),
            "resend_last_tx on a completed QSO must resend at the audio-offset frequency, not \
             the ~14 MHz RF frequency — the TX worker's modulation limit would reject that and \
             the message would never actually key"
        );
    }

    /// Companion to the test above: the SAME dial (no band change) must
    /// still correctly suppress, proving the dial-adjustment doesn't
    /// accidentally defeat the original PAN-14 same-band protection.
    #[tokio::test]
    async fn recently_completed_qso_on_the_same_dial_band_still_reserves_the_sender() {
        let mut manager = QsoManager::new(test_config());
        let dial = std::sync::Arc::new(AtomicU64::new(14_074_000)); // 20m, unchanged throughout
        manager.set_dial_frequency_source(dial);
        let audio_offset = 1500.0;

        let qso_a = manager.start_cq(audio_offset, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                audio_offset,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                audio_offset,
                Some(-11.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_a).await.unwrap().state,
            QsoState::Completed { .. }
        ));

        let qso_b = manager
            .start_cq(audio_offset + 5.0, None, false)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                audio_offset + 5.0,
                Some(-10.0),
            )
            .await
            .unwrap();

        let p_b = manager.get_qso(qso_b).await.unwrap();
        assert!(
            matches!(p_b.state, QsoState::CallingCq { .. }),
            "qso_b must remain un-advanced — K1DEF is reserved by its recent same-band \
             completion, got {:?}",
            p_b.state
        );
    }

    /// Layer 2 timeline persistence (docs/observability-diagnostics-plan.md):
    /// the `QsoEvent::QsoCompleted` broadcast at the end of a real multi-step
    /// exchange must carry the QSO's ACTUAL state_history/messages, not the
    /// empty vecs `QsoLogger::handle_qso_completed` used to hard-code. This
    /// is what proves the emission sites (not just the DB round-trip) are
    /// wired correctly — a persistence subscriber never sees an empty
    /// timeline for a QSO that genuinely had one.
    #[tokio::test]
    async fn qso_completed_event_carries_full_timeline() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq(freq, None, false).await.unwrap();
        let mut rx = manager.subscribe();

        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let completed = events
            .into_iter()
            .find_map(|e| match e {
                QsoEvent::QsoCompleted {
                    qso_id: id,
                    state_history,
                    messages,
                    ..
                } if id == qso_id => Some((state_history, messages)),
                _ => None,
            })
            .expect("expected a QsoCompleted event for this qso_id");
        let (state_history, messages) = completed;

        // Three forward transitions: CallingCq->WaitingForReport->
        // WaitingForConfirmation->Completed, one per process_message call.
        assert_eq!(
            state_history.len(),
            3,
            "expected 3 state transitions in the completed event, got {:?}",
            state_history
        );
        assert!(
            matches!(state_history[0].from_state, QsoState::CallingCq { .. }),
            "first transition must start from CallingCq, got {:?}",
            state_history[0]
        );
        assert!(
            matches!(
                state_history.last().unwrap().to_state,
                QsoState::Completed { .. }
            ),
            "last transition must land on Completed, got {:?}",
            state_history.last()
        );
        // At minimum the CQ we sent plus the three replies we sent in
        // response — never empty like the old hard-coded discard.
        assert!(
            !messages.is_empty(),
            "expected a non-empty message history in the completed event"
        );
    }

    /// The manual CQer also EMITS the right reply at each step (the auto-reply
    /// path is Manual-gated). Drive a manual CQ QSO and verify the reply
    /// sequence SignalReport → FinalConfirmation reaches the event bus.
    #[tokio::test]
    async fn manual_cqer_emits_report_then_rr73() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        // Build a manual CallingCq QSO directly (start_cq is Auto-only; the
        // operator-CQ-as-QSO wiring is a separate deferred item — see report).
        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let progress = QsoProgress {
            state: QsoState::CallingCq {
                frequency: freq,
                started_at: now,
                call_count: 1,
            },
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id,
                our_callsign: "W1ABC".to_string(),
                their_callsign: None,
                frequency: freq,
                completed_rf_frequency_hz: None,
                mode: "FT8".to_string(),
                start_time: now,
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
                contest_info: None,
                tags: HashMap::new(),
                notes: None,
                tx_parity: None,
                initiated_by: CallInitiation::Manual,
                role: QsoRole::Cqer,
                call_count: 1,
                first_call_at: Some(now),
                last_call_at: Some(now),
                progressed_this_cycle: false,
                stall_cycles: 0,
                last_known_good_offset_hz: None,
                advance_generation: 0,
                pre_switch_offset: None,
                hound: false,
                partner_freq: None,
                pending_freq_drift: None,
                hound_qsyed: false,
                remote_origin: false,
                tx_parity_provisional: false,
            },
        };
        manager.qsos.write().await.insert(qso_id, progress);

        let mut events = manager.subscribe();

        // Caller answers → we should emit a SignalReport.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        let mut saw_report = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::SignalReport { .. }) {
                        saw_report = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(saw_report, "manual CQer should emit a SignalReport reply");

        // Caller R-reports → we should emit a FinalConfirmation (RR73).
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let mut saw_rr73 = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::FinalConfirmation { .. }) {
                        saw_rr73 = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(
            saw_rr73,
            "manual CQer should emit a FinalConfirmation (RR73)"
        );
    }

    // --- Manual `c` (CQ) → CallingCq QSO ---------------------------------

    /// Pressing `c` (`start_cq_manual`) creates an ACTIVE, manual CallingCq
    /// QSO that emits a StateChanged (so the coordinator keys it into
    /// `active_tx_qsos`) and an opening Cq MessageToSend.
    #[tokio::test]
    async fn start_cq_manual_creates_active_calling_cq_qso() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let mut events = manager.subscribe();

        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();

        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::CallingCq { .. }),
            "expected CallingCq, got {:?}",
            p.state
        );
        assert_eq!(p.metadata.initiated_by, CallInitiation::Manual);
        assert_eq!(p.metadata.role, QsoRole::Cqer);
        // It is an active QSO (so it shows in the active set).
        assert_eq!(manager.get_active_qsos().await.len(), 1);

        // It emitted a StateChanged into CallingCq (keys active_tx_qsos in the
        // coordinator) and an opening Cq MessageToSend.
        let mut saw_state_change = false;
        let mut saw_cq = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::StateChanged { new_state, .. }) => {
                    if matches!(new_state, QsoState::CallingCq { .. }) {
                        saw_state_change = true;
                    }
                }
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::Cq { .. }) {
                        saw_cq = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(
            saw_state_change,
            "start_cq_manual should emit a StateChanged into CallingCq"
        );
        assert!(saw_cq, "start_cq_manual should emit an opening Cq message");
    }

    /// The full operator-CQ exchange: a caller answers our manual CQ and the
    /// exchange auto-sequences (Manual-gated auto-reply) all the way to
    /// Completed + QsoCompleted (ADIF log), latching the caller's grid.
    #[tokio::test]
    async fn start_cq_manual_caller_answer_completes_and_logs() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config()); // our call = W1ABC
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();
        let mut events = manager.subscribe();

        // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager.get_qso(qso_id).await.unwrap().state,
                QsoState::WaitingForReport { .. }
            ),
            "CallingCq + CqResponse → WaitingForReport"
        );

        // Caller rogers our report: "W1ABC K1DEF R-12".
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();

        // Caller closes with 73 → Completed.
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();

        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            p.state
        );
        assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
        assert!(p.metadata.end_time.is_some());

        // A QsoCompleted event fired (ADIF logger subscribes to this).
        let mut saw_completed = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::QsoCompleted { qso_id: id, .. }) if id == qso_id => {
                    saw_completed = true;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(saw_completed, "completed CQ QSO should emit QsoCompleted");
    }

    /// While calling CQ (un-answered), the per-slot rearm keeps re-emitting our
    /// CQ — exactly ONE keep-call per slot (no double-TX), bounded by the
    /// manual watchdog.
    #[tokio::test]
    async fn manual_cq_rearm_re_emits_one_cq_per_slot() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();
        let mut events = manager.subscribe();

        let start = Utc::now();
        // Same slot as creation (< 15s elapsed): rearm must NOT re-emit (the
        // opening CQ already went out — re-emitting now would double-TX).
        manager
            .rearm_manual_calls_at(start + Duration::seconds(5))
            .await;
        // One slot later: rearm re-emits exactly one Cq.
        manager
            .rearm_manual_calls_at(start + Duration::seconds(16))
            .await;

        let mut cq_count = 0;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::Cq { .. }) {
                        cq_count += 1;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert_eq!(
            cq_count, 1,
            "rearm should emit exactly one CQ keep-call across one elapsed slot"
        );

        // Sanity: the QSO is still CallingCq and call_count advanced.
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(p.state, QsoState::CallingCq { .. }));
        assert_eq!(p.metadata.call_count, 2, "1 opening + 1 rearm");
    }

    /// docs/qso-engine-bugs.md BUG 1: a `tx_parity: None` (no fixed
    /// preference — the Auto-config default) CallingCq QSO MUST latch a
    /// single concrete parity at creation and hold it for the life of the
    /// QSO — the opening CQ and EVERY keep-call rearm across many slots must
    /// all carry the SAME parity. Before the fix, `tx_parity` stayed `None`
    /// and each emission re-asked "nearest slot" fresh, alternating parities
    /// (i.e. transmitting on both — the station never heard replies).
    #[tokio::test]
    async fn manual_cq_with_no_parity_preference_latches_one_parity_for_life_of_qso() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();

        // The opening CQ must have latched a CONCRETE (not None) parity.
        let latched = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .tx_parity
            .expect(
                "start_cq_manual(tx_parity: None) must latch a concrete parity, not leave it None",
            );

        let mut events = manager.subscribe();
        let start = Utc::now();

        // Drive 6 consecutive keep-call rearms (6 slots = 90s of CQing).
        for slot in 1..=6i64 {
            manager
                .rearm_manual_calls_at(start + Duration::seconds(15 * slot + 1))
                .await;
        }

        let mut cq_parities = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend {
                message: MessageType::Cq { .. },
                tx_parity,
                ..
            } = ev
            {
                cq_parities.push(tx_parity);
            }
        }
        assert_eq!(cq_parities.len(), 6, "one CQ per slot across 6 slots");
        for (i, p) in cq_parities.iter().enumerate() {
            assert_eq!(
                *p,
                Some(latched),
                "rearm #{i} transmitted on a DIFFERENT parity than the opening CQ \
                 — this is BUG 1: transmitting on every window instead of alternating"
            );
        }
    }

    /// A caller-supplied FIXED parity preference (`Some(_)`) must be honored
    /// as-is — the latch-on-None fix must not override an explicit choice.
    #[tokio::test]
    async fn manual_cq_with_explicit_parity_preference_is_not_overridden() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .start_cq_manual(
                14074000.0,
                Some(pancetta_core::slot::SlotParity::Odd),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.tx_parity,
            Some(pancetta_core::slot::SlotParity::Odd),
            "an explicit parity preference must be honored unchanged"
        );
    }

    /// The manual CQ watchdog retires an un-answered CallingCq QSO once it
    /// hits the max-calls bound (so we never CQ forever).
    #[tokio::test]
    async fn manual_cq_watchdog_retires_after_max_calls() {
        // Pin a small call cap so this exercises the cap mechanism
        // deterministically, independent of the (now 25) default.
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 10;
        let manager = QsoManager::new(config);
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();

        // Drive enough slots to exceed manual_call_max_calls (10).
        let mut t = Utc::now();
        for _ in 0..15 {
            t += Duration::seconds(16);
            manager.rearm_manual_calls_at(t).await;
        }
        manager.check_timeouts_at(t).await;

        // QSO retired (Failed / mapping cleared → not found via get_qso after
        // cleanup, or in a terminal state).
        match manager.get_qso(qso_id).await {
            Ok(p) => assert!(
                p.state.is_terminal(),
                "watchdog should retire the CQ QSO, got {:?}",
                p.state
            ),
            Err(QsoManagerError::QsoNotFound { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// `cancel_qso` (StopCq path) cancels an un-answered CallingCq QSO.
    #[tokio::test]
    async fn manual_cq_cancel_stops_calling() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None, false).await.unwrap();
        assert_eq!(manager.get_active_qsos().await.len(), 1);

        manager.cancel_qso(qso_id).await.unwrap();

        // No longer active; a subsequent rearm emits nothing for it.
        assert!(manager
            .get_active_qsos()
            .await
            .iter()
            .all(|(_, p)| !matches!(p.state, QsoState::CallingCq { .. })));
    }

    // --- Batch 2 #7 / FIX 1: case-insensitive continue --------------------

    /// A manual call to "k1def" must CONTINUE the existing active QSO with
    /// "K1DEF" (case-insensitive match), not spawn a duplicate and not
    /// supersede. The case-insensitive callsign keying is what makes the
    /// FIX-1 active-QSO lookup robust to case/format mismatches between a
    /// DX-Hunter call and a Callers reply for the same station.
    #[tokio::test]
    async fn recall_continue_is_case_insensitive() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();
        // Re-call with different case — should CONTINUE `first` (same id).
        let second = manager
            .respond_to_cq_manual("k1def".to_string(), freq, None)
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "case-different re-call must continue the same QSO"
        );
        // `first` is NOT superseded — still active.
        let first_state = manager.get_qso(first).await.unwrap().state;
        assert!(
            matches!(first_state, QsoState::RespondingToCq { .. }),
            "first QSO must remain active (not superseded), got {:?}",
            first_state
        );
        let active = manager.get_active_qsos().await;
        assert_eq!(
            active.len(),
            1,
            "exactly one active QSO after case-different re-call"
        );
        assert_eq!(active[0].0, first);
    }

    // --- Batch 2 #8: grid latched into Completed (Caller path) ------------

    /// In the Caller flow the DX's grid arrives in the opening CqResponse and
    /// must reach the logged metadata even though the close arm hard-codes
    /// grid_square: None. Drive a manual caller QSO and complete it.
    #[tokio::test]
    async fn caller_grid_latched_into_completed_metadata() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();

        // The DX re-sends "W1ABC K1DEF FN31" (their grid) — latch it.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();

        // DX sends our report → SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -9,
                },
                "W1ABC K1DEF -09".to_string(),
                freq,
                Some(-9.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));

        // DX closes from our R directly with RR73 → Completed (grid_square None
        // in the state, but metadata.grids.theirs already latched FN31).
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF RR73".to_string(),
                freq,
                Some(-9.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "got {:?}",
            p.state
        );
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
    }
}

#[cfg(test)]
mod sender_verification_tests {
    use super::*;
    use chrono::Utc;

    fn manager_with_call(our: &str) -> QsoManager {
        let config = QsoManagerConfig {
            our_callsign: our.into(),
            our_grid: Some("FN42".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        };
        QsoManager::new(config)
    }

    /// Minimal `QsoMetadata` for unit tests: `partner_freq = None` (normal QSO,
    /// no Hound split). All tests that call `is_message_relevant` directly must
    /// pass a metadata; this helper ensures the regression cases use the
    /// canonical "no partner_freq" path, matching pre-Hound behavior.
    fn normal_metadata(our: &str, their_freq: f64) -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id: uuid::Uuid::new_v4(),
            our_callsign: our.into(),
            their_callsign: None,
            frequency: their_freq,
            completed_rf_frequency_hz: None,
            mode: "FT8".into(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: CallInitiation::default(),
            role: QsoRole::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
        }
    }

    #[tokio::test]
    async fn spoofed_signal_report_does_not_advance_state() {
        let manager = manager_with_call("K5ARH");
        let mut events = manager.subscribe();
        let qso_id = Uuid::new_v4();
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        // Attacker sends a properly-addressed report from a DIFFERENT call.
        let spoof = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -12,
        };
        let new_state = manager
            .determine_state_transition(qso_id, &state, &spoof, None, CallInitiation::Auto, false)
            .await
            .unwrap();
        // State must NOT advance.
        assert!(matches!(new_state, QsoState::RespondingToCq { .. }));
        match events.try_recv().expect("security rejection event") {
            QsoEvent::MessageRejected {
                qso_id: got,
                reason,
                from_callsign,
                to_callsign,
            } => {
                assert_eq!(got, qso_id);
                assert_eq!(reason, RejectionReason::SenderNotPartner);
                assert_eq!(from_callsign.as_deref(), Some("NF4KE"));
                assert_eq!(to_callsign.as_deref(), Some("K5ARH"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn legitimate_signal_report_advances_state() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        let new_state = manager
            .determine_state_transition(
                Uuid::new_v4(),
                &state,
                &legit,
                None,
                CallInitiation::Auto,
                false,
            )
            .await
            .unwrap();
        assert!(
            matches!(new_state, QsoState::SendingReport { .. }),
            "expected SendingReport, got {:?}",
            new_state
        );
    }

    #[test]
    fn is_message_relevant_rejects_spoofed_sender() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let spoof = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -12,
        };
        // partner_freq = None → regression path (normal QSO behavior unchanged).
        let md = normal_metadata("K5ARH", 1500.0);
        assert!(!manager.is_message_relevant(&state, &md, &spoof, 1500.0, false));
    }

    #[test]
    fn classify_relevance_reports_only_entangled_security_traffic() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let metadata = normal_metadata("K5ARH", 1500.0);
        let impostor = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -12,
        };
        let verdict = manager.classify_relevance(&state, &metadata, &impostor, 1500.0, false);
        assert!(!verdict.relevant);
        assert_eq!(verdict.reason, Some(RejectionReason::SenderNotPartner));

        for (from_station, to_station, frequency, description) in [
            ("K9ZZ", "W1AW", 1500.0, "partner working a third party"),
            ("NF4KE", "W1AW", 1500.0, "unrelated third-party traffic"),
            ("NF4KE", "K5ARH", 2400.0, "off-frequency impostor traffic"),
        ] {
            let ordinary_traffic = MessageType::SignalReport {
                to_station: to_station.into(),
                from_station: from_station.into(),
                report: -12,
            };
            let verdict =
                manager.classify_relevance(&state, &metadata, &ordinary_traffic, frequency, false);
            assert!(!verdict.relevant, "{description}");
            assert_eq!(verdict.reason, None, "{description} must stay silent");
        }
    }

    #[tokio::test]
    async fn message_rejected_event_is_deliverable_to_subscribers() {
        let manager = manager_with_call("K5ARH");
        let mut events = manager.subscribe();
        let qso_id = Uuid::new_v4();
        manager
            .emit_event(QsoEvent::MessageRejected {
                qso_id,
                reason: RejectionReason::SenderNotPartner,
                from_callsign: Some("BOGUS9".into()),
                to_callsign: Some("K5ARH".into()),
            })
            .await;
        assert!(matches!(
            events.try_recv(),
            Ok(QsoEvent::MessageRejected { qso_id: got, .. }) if got == qso_id
        ));
    }

    #[test]
    fn is_message_relevant_accepts_legitimate_sender() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None → regression path (normal QSO behavior unchanged).
        let md = normal_metadata("K5ARH", 1500.0);
        assert!(manager.is_message_relevant(&state, &md, &legit, 1500.0, false));
    }

    #[test]
    fn is_message_relevant_tight_gate_for_initial_ambiguous_match() {
        // The tight 15 Hz gate still governs INITIAL / ambiguous matching —
        // a state with no known contra callsign (CallingCq). This preserves
        // the security-review C-1 tightening for the case where we have not
        // yet locked onto a partner. (B15 only widens the gate once a QSO is
        // ESTABLISHED; see is_message_relevant_established_qso_allows_drift.)
        let manager = manager_with_call("K5ARH");
        let state = QsoState::CallingCq {
            frequency: 1500.0,
            started_at: Utc::now(),
            call_count: 1,
        };
        // A bare report answering our CQ (A4 routing shape), addressed to us.
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None → regression path (normal QSO behavior unchanged).
        let md = normal_metadata("K5ARH", 1500.0);
        // 16 Hz off, no partner latched yet → rejected (tight gate).
        assert!(!manager.is_message_relevant(&state, &md, &legit, 1516.0, false));
        // 14 Hz off → accepted.
        assert!(manager.is_message_relevant(&state, &md, &legit, 1514.0, false));
    }

    #[test]
    fn is_message_relevant_established_qso_allows_drift() {
        // B15: once a QSO is ESTABLISHED (contra callsign known, here a
        // RespondingToCq partner answering us with from+to+state all matching),
        // callsign+state continuity wins over the tight 15 Hz window — a DX that
        // drifted beyond 15 Hz is still routed (up to the 100 Hz established
        // bound) instead of being dropped, so an actively-answering partner can
        // complete the contact. The old 50 Hz tolerance is gone for the initial
        // case, but the established case now intentionally accepts up to 100 Hz.
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None → regression path (normal QSO behavior unchanged).
        let md = normal_metadata("K5ARH", 1500.0);
        // 16 Hz, 45 Hz, 100 Hz drift from an established partner → accepted.
        assert!(manager.is_message_relevant(&state, &md, &legit, 1516.0, false));
        assert!(manager.is_message_relevant(&state, &md, &legit, 1545.0, false));
        assert!(manager.is_message_relevant(&state, &md, &legit, 1600.0, false));
        // Beyond the 100 Hz established bound → rejected (still bounded).
        assert!(!manager.is_message_relevant(&state, &md, &legit, 1601.0, false));
    }

    // ── Hound partner_freq routing ───────────────────────────────────────────

    /// Hound QSO: the QSO's TX offset (`frequency`) is low (600 Hz, where Hounds
    /// call), but the Fox replies on a different, higher offset (`partner_freq =
    /// Some(1800.0)`). A frame arriving at the Fox's frequency IS relevant; a
    /// frame arriving at the Hound's TX offset but far from the Fox's RX offset
    /// is NOT relevant.
    #[test]
    fn is_message_relevant_hound_keys_on_partner_freq() {
        let manager = manager_with_call("K5ARH");
        // Hound state: our TX offset is low (600 Hz); Fox's RX offset is 1800 Hz.
        let state = QsoState::RespondingToCq {
            target_callsign: "KH8B".into(),
            frequency: 600.0, // our TX offset (Hound calls low)
            started_at: Utc::now(),
        };
        let fox_report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "KH8B".into(),
            report: -10,
        };
        // Hound metadata: partner_freq = Some(1800.0) (Fox's RX offset).
        let mut md = normal_metadata("K5ARH", 600.0);
        md.partner_freq = Some(1800.0);

        // Frame at the Fox's offset (1800 Hz) — within ESTABLISHED tolerance.
        // The gate must match against partner_freq (1800), NOT our TX freq (600).
        assert!(
            manager.is_message_relevant(&state, &md, &fox_report, 1800.0, false),
            "Hound: frame at Fox's offset must be relevant"
        );
        // Frame at 1810 Hz — still within 100 Hz ESTABLISHED bound of 1800.
        assert!(
            manager.is_message_relevant(&state, &md, &fox_report, 1810.0, false),
            "Hound: frame 10 Hz from Fox's offset must be relevant (within established tolerance)"
        );
        // Frame at OUR TX offset (600 Hz) but far from the Fox's RX offset —
        // must be rejected because 600 Hz is not close to partner_freq 1800 Hz.
        assert!(
            !manager.is_message_relevant(&state, &md, &fox_report, 600.0, false),
            "Hound: frame at our TX offset (far from Fox) must NOT be relevant"
        );
        // Frame beyond even the ESTABLISHED bound from partner_freq — rejected.
        assert!(
            !manager.is_message_relevant(&state, &md, &fox_report, 1901.0, false),
            "Hound: frame >100 Hz from Fox's offset must NOT be relevant"
        );
    }

    /// Regression: with `partner_freq = None` (every normal QSO), the gate
    /// falls back to `state.frequency()` exactly as before — byte-identical
    /// behavior. A frame at the QSO's latched frequency is relevant; one far
    /// away is not.
    #[test]
    fn is_message_relevant_partner_freq_none_falls_back_to_state_freq() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None — normal QSO regression path.
        let md = normal_metadata("K5ARH", 1500.0);
        // Frame at the QSO frequency → relevant (unchanged from pre-Hound).
        assert!(
            manager.is_message_relevant(&state, &md, &legit, 1500.0, false),
            "regression: frame at state.frequency must be relevant when partner_freq=None"
        );
        // Frame far from QSO frequency → not relevant (unchanged).
        assert!(
            !manager.is_message_relevant(&state, &md, &legit, 2000.0, false),
            "regression: frame far from state.frequency must NOT be relevant when partner_freq=None"
        );
    }

    /// 2026-07-26 incident regression (v2, two-strike confirm): a single off-latch
    /// SignalReport must NOT advance the QSO — it only notes a pending drift
    /// candidate. This must be byte-identical to today's existing (unmodified) drop
    /// behavior; `is_message_relevant`/`determine_state_transition` are untouched by
    /// this fix, so this test is really proving the new pre-pass doesn't change
    /// first-sighting behavior at all.
    #[tokio::test]
    async fn single_off_frequency_sighting_does_not_advance_or_relatch() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-19.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a single off-frequency sighting must not advance the QSO; got {:?}",
            progress.state
        );
        assert!(
            matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 937.5),
            "the first sighting must be noted as a pending drift candidate"
        );
    }

    /// The confirming second sighting at the SAME new frequency relatches and lets the
    /// QSO advance normally through the completely-unmodified existing pipeline —
    /// exactly the real 2026-07-26 LU7LRP timeline (two SignalReport decodes at
    /// 937.5 Hz, ~30s apart).
    #[tokio::test]
    async fn second_matching_sighting_confirms_and_relatches() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-19.0),
            )
            .await
            .unwrap();
        // Confirmation now requires a real >=5s gap from the ORIGINAL candidate
        // timestamp (final-review Critical fix), so the confirming sighting must
        // land after a genuine delay, not back-to-back with the first.
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-17.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "the confirmed second sighting must advance the QSO; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.frequency, 937.5,
            "metadata.frequency must relatch to the confirmed frequency"
        );
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "pending_freq_drift must clear once confirmed"
        );
        if let QsoState::SendingReport { frequency, .. } = progress.state {
            assert_eq!(
                frequency, 937.5,
                "the state's own embedded frequency field must also relatch \
                 (is_message_relevant reads this field, not metadata.frequency)"
            );
        }
    }

    /// A different second frequency does NOT confirm — the candidate simply resets to
    /// the newest off-latch sighting instead, and the QSO stays stuck (matching
    /// today's behavior). This is the direct proof this mechanism can't be tricked by
    /// two DIFFERENT spoofed frequencies in a row either.
    #[tokio::test]
    async fn different_second_frequency_does_not_confirm() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        for freq in [937.5, 1100.0] {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: "K5ARH".into(),
                        from_station: "LU7LRP".into(),
                        report: -11,
                    },
                    "K5ARH LU7LRP -11".into(),
                    freq,
                    Some(-19.0),
                )
                .await
                .unwrap();
        }

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a differing second sighting must not confirm/advance; got {:?}",
            progress.state
        );
        assert!(
            matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 1100.0),
            "the candidate must reset to the newest off-latch sighting"
        );
    }

    /// A normal in-tolerance message arriving after a pending candidate clears it —
    /// no spurious relatch or advance from a stale candidate once the drift resolves
    /// itself (or was noise).
    #[tokio::test]
    async fn in_tolerance_message_clears_a_stale_pending_candidate() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-19.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .pending_freq_drift,
            Some((f, _)) if f == 937.5
        ));

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -9,
                },
                "K5ARH LU7LRP -09".into(),
                1550.0,
                Some(-12.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "the in-tolerance message must advance the QSO normally; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "the stale candidate must clear once a normal in-tolerance message arrives"
        );
    }

    /// A passband-violating decode must not overwrite a legitimate pending candidate —
    /// it's likely a garbage decode, not a real drift signal, and shouldn't reset real
    /// tracking. The genuine confirming sighting must still work afterward.
    #[tokio::test]
    async fn out_of_passband_decode_does_not_reset_pending_candidate() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-19.0),
            )
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                -50.0,
                Some(-25.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager
                    .get_qso(qso_id)
                    .await
                    .unwrap()
                    .metadata
                    .pending_freq_drift,
                Some((f, _)) if f == 937.5
            ),
            "an out-of-passband decode must not overwrite a legitimate pending candidate"
        );

        // Confirmation now requires a real >=5s gap from the ORIGINAL candidate
        // timestamp (final-review Critical fix), so the confirming sighting must
        // land after a genuine delay, not back-to-back with the earlier sightings.
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-17.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager.get_qso(qso_id).await.unwrap().state,
                QsoState::SendingReport { .. }
            ),
            "the real confirming sighting must still relatch and advance after the noise"
        );
    }

    /// Final-review Critical finding: the hb-091 scoped fast-path can forward two decode
    /// copies of the SAME physical transmission to the QSO component milliseconds apart
    /// (dedup previously relied on is_message_relevant, which this pre-pass runs before).
    /// Two same-instant deliveries of one transmission must NOT confirm a relatch -- only a
    /// sighting that's genuinely >=5s after the ORIGINAL candidate may confirm.
    #[tokio::test]
    async fn duplicate_delivery_of_same_transmission_does_not_confirm() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let t0 = Utc::now();
        // Two decode-pipeline copies of the SAME transmission, milliseconds apart --
        // mirrors the hb-091 scoped fast-path + standard pipeline both forwarding the
        // same window's content to the QSO component.
        manager
            .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(
                &report,
                937.5,
                t0 + chrono::Duration::milliseconds(50),
            )
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "two same-instant deliveries of ONE transmission must not confirm/relatch"
        );
        assert!(
            matches!(progress.metadata.pending_freq_drift, Some((f, t)) if f == 937.5 && t == t0),
            "the pending candidate must keep its ORIGINAL timestamp, not be pushed forward \
             by the duplicate delivery -- otherwise repeated fast deliveries could delay \
             confirmation indefinitely"
        );

        // A genuinely later sighting (>=5s after the ORIGINAL t0, not the duplicate) confirms.
        manager
            .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::seconds(6))
            .await;
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.frequency, 937.5,
            "a sighting >=5s after the original candidate must confirm and relatch"
        );
        assert_eq!(progress.metadata.pending_freq_drift, None);
    }

    /// Boundary case for the 5s confirm gap: a sighting just under the gate must NOT
    /// confirm, and must leave the pending candidate's ORIGINAL timestamp untouched
    /// (proving the non-reset-on-duplicate rule holds right at the boundary, where a
    /// future refactor is most likely to break it).
    #[tokio::test]
    async fn sighting_just_under_the_confirm_gap_does_not_confirm() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(
                &report,
                937.5,
                t0 + chrono::Duration::milliseconds(4900),
            )
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a sighting 4.9s after the original must not confirm/relatch"
        );
        assert!(
            matches!(progress.metadata.pending_freq_drift, Some((f, t)) if f == 937.5 && t == t0),
            "the candidate must still carry the ORIGINAL timestamp, unchanged"
        );
    }

    /// Closes the boundary from the other side: a sighting at EXACTLY 5.0s (the
    /// inclusive edge of `DRIFT_CONFIRM_MIN_GAP`) must confirm. Combined with
    /// `sighting_just_under_the_confirm_gap_does_not_confirm` (4.9s, must NOT confirm),
    /// this pins the gate to `>=`, not `>` -- either an accidental `>` or a shift of
    /// the constant itself (e.g. 5s -> 6s) would flip one of these two tests.
    #[tokio::test]
    async fn sighting_at_exactly_the_confirm_gap_does_confirm() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::seconds(5))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.frequency, 937.5,
            "a sighting at EXACTLY the 5s gap must confirm and relatch (inclusive bound)"
        );
        assert_eq!(progress.metadata.pending_freq_drift, None);
    }

    /// Final-review Important finding: the drift-candidate eligibility bound must be
    /// the wide RX-plausibility range (200-2900 Hz), NOT this file's narrower
    /// TX_OFFSET_MIN_HZ/MAX_HZ (300-2700 Hz, which govern autonomous fresh-offset
    /// picking, not where a real DX might legitimately be heard). Pins BOTH edges so a
    /// future refactor that silently narrows this back to TX_OFFSET_* is caught —
    /// exactly the un-caught regression that produced this test. A DX replying from
    /// near either edge of the real passband must still be able to confirm and
    /// relatch.
    #[tokio::test]
    async fn drift_candidate_confirms_near_the_passband_edges() {
        let manager = manager_with_call("K5ARH");

        // Lower edge: 250 Hz is inside DRIFT_CANDIDATE_MIN_HZ..=MAX_HZ (200-2900) but
        // outside TX_OFFSET_MIN_HZ..=MAX_HZ (300-2700) -- if the eligibility check ever
        // regresses back to the TX bounds, this confirm silently stops happening.
        let qso_low = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();
        let report_low = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report_low, 250.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report_low, 250.0, t0 + chrono::Duration::seconds(6))
            .await;
        let progress = manager.get_qso(qso_low).await.unwrap();
        assert_eq!(
            progress.metadata.frequency, 250.0,
            "a DX at 250 Hz (inside the RX-plausibility bound, outside TX_OFFSET_*) must \
             still be able to confirm and relatch"
        );

        // Upper edge: 2850 Hz, same reasoning.
        let qso_high = manager
            .respond_to_cq_manual("VK9XYZ".into(), 1500.0, None)
            .await
            .unwrap();
        let report_high = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "VK9XYZ".into(),
            report: -9,
        };
        let t1 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report_high, 2850.0, t1)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(
                &report_high,
                2850.0,
                t1 + chrono::Duration::seconds(6),
            )
            .await;
        let progress = manager.get_qso(qso_high).await.unwrap();
        assert_eq!(
            progress.metadata.frequency, 2850.0,
            "a DX at 2850 Hz (inside the RX-plausibility bound, outside TX_OFFSET_*) must \
             still be able to confirm and relatch"
        );
    }

    /// Pins the OUTER upper bound of DRIFT_CANDIDATE_MAX_HZ (2900 Hz) -- a decode past
    /// it is implausible/garbage and must never become (or confirm) a candidate.
    #[tokio::test]
    async fn drift_candidate_rejects_outside_the_rx_plausibility_bound() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 2950.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 2950.0, t0 + chrono::Duration::seconds(6))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a decode past the RX-plausibility bound must never confirm/relatch"
        );
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "an implausible decode must never even become a pending candidate"
        );
    }

    /// PAN-12 / issue #245: an ordinary split-TX QSO created by the TX ceiling
    /// clamp relatches where it hears the DX while preserving our TX offset.
    #[tokio::test]
    async fn clamped_split_tx_qso_relatches_partner_freq_on_confirmed_drift() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_with(
                "K6L".into(),
                2700.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(2931.0),
                false,
            )
            .await
            .unwrap();

        for (frequency, snr) in [(855.0, -19.0), (856.0, -17.0)] {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: "K5ARH".into(),
                        from_station: "K6L".into(),
                        report: -11,
                    },
                    "K5ARH K6L -11".into(),
                    frequency,
                    Some(snr),
                )
                .await
                .unwrap();

            if frequency == 855.0 {
                let progress = manager.get_qso(qso_id).await.unwrap();
                assert!(
                    matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 855.0),
                    "a clamped split-TX QSO must note a drift candidate"
                );
                assert_eq!(progress.metadata.partner_freq, Some(2931.0));
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            }
        }

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(progress.metadata.partner_freq, Some(856.0));
        assert_eq!(progress.metadata.frequency, 2700.0);
        assert_eq!(progress.metadata.pending_freq_drift, None);
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        if let QsoState::SendingReport { frequency, .. } = progress.state {
            assert_eq!(frequency, 2700.0);
        }
    }

    /// A split-TX relatch must announce itself on the event bus.
    ///
    /// The coordinator refreshes its scoped/AP decoder hint
    /// (`active_qso_freq_hz`) only inside the `QsoEvent::StateChanged` arm,
    /// where it re-reads `metadata.partner_freq`. A relatch that mutated
    /// `partner_freq` silently therefore left the decoder centred on the
    /// obsolete partner offset — while the relevance gate had already moved to
    /// the new one — until some unrelated transition happened to fire. Two
    /// repeated `SignalReport` frames leave the QSO in `SendingReport`, so
    /// there is no such transition to piggyback on.
    #[tokio::test]
    async fn split_tx_relatch_emits_state_change_for_decoder_refresh() {
        use tokio::sync::broadcast::error::TryRecvError;

        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_with(
                "K6L".into(),
                2700.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(2931.0),
                false,
            )
            .await
            .unwrap();
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K6L".into(),
            report: -11,
        };

        // Subscribe AFTER establishment so only relatch-driven events are seen.
        let mut events = manager.subscribe();
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 855.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 856.0, t0 + chrono::Duration::seconds(6))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(progress.metadata.partner_freq, Some(856.0));

        let mut saw_state_change = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::StateChanged {
                    qso_id: changed, ..
                }) => {
                    if changed == qso_id {
                        saw_state_change = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(
            saw_state_change,
            "a confirmed split-TX relatch must emit StateChanged so the coordinator \
             re-reads partner_freq into its decoder hint"
        );
    }

    /// Once split-TX drift relatches, the relevance gate follows the new
    /// partner location and rejects the obsolete one.
    #[tokio::test]
    async fn relatched_partner_freq_routes_subsequent_frames() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_with(
                "K6L".into(),
                2700.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(2931.0),
                false,
            )
            .await
            .unwrap();
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K6L".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 855.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 856.0, t0 + chrono::Duration::seconds(6))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(manager.is_message_relevant(
            &progress.state,
            &progress.metadata,
            &report,
            856.0,
            false
        ));
        assert!(!manager.is_message_relevant(
            &progress.state,
            &progress.metadata,
            &report,
            2931.0,
            false
        ));
    }

    /// A healthy held-offset QSO measures drift from partner_freq, not our TX.
    #[tokio::test]
    async fn held_offset_qso_with_dx_on_partner_freq_never_drifts() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_with(
                "VK4DX".into(),
                1500.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(700.0),
                false,
            )
            .await
            .unwrap();
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "VK4DX".into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 700.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 700.0, t0 + chrono::Duration::seconds(6))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(progress.metadata.pending_freq_drift, None);
        assert_eq!(progress.metadata.partner_freq, Some(700.0));
        assert_eq!(progress.metadata.frequency, 1500.0);
    }

    /// Genuine Hound/Fox retains its protocol-specific drift carve-out.
    #[tokio::test]
    async fn genuine_hound_qso_is_still_skipped_by_drift_confirm() {
        let manager = manager_with_call("K5ARH");
        let mut events = manager.subscribe();
        let qso_id = manager
            .engage_hound(
                "D2UY",
                1800.0,
                Some("JI64"),
                Some(pancetta_core::slot::SlotParity::Even),
            )
            .await
            .unwrap();
        while events.try_recv().is_ok() {}

        // PAN-15 item 4: no real-time sleep between these two off-Fox frames.
        // The Hound skip in `maybe_confirm_frequency_drift_at` returns before
        // any drift candidate is ever recorded (`pending_freq_drift` stays
        // `None` throughout), so the >=5s two-strike confirmation gap can't
        // influence the outcome here — a sleep between the frames was
        // provably dead time.
        for snr in [-19.0, -17.0] {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: "K5ARH".into(),
                        from_station: "D2UY".into(),
                        report: -12,
                    },
                    "K5ARH D2UY -12".into(),
                    900.0,
                    Some(snr),
                )
                .await
                .unwrap();
        }

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(progress.metadata.pending_freq_drift, None);
        assert_eq!(progress.metadata.partner_freq, Some(1800.0));
        assert!(!progress.metadata.hound_qsyed);
        assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
        assert!(
            events.try_recv().is_err(),
            "off-Fox frames must not advance, QSY, or emit a TX event"
        );

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -12,
                },
                "K5ARH D2UY -12".into(),
                1800.0,
                Some(-15.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(progress.metadata.hound_qsyed);
        assert_eq!(progress.metadata.partner_freq, Some(1800.0));
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
    }

    /// PAN-15 item 2 regression: `engage_hound` calls `respond_to_cq_with`
    /// (which sets `partner_freq` but leaves `hound=false`) and only stamps
    /// `hound=true` in a SEPARATE later write-lock acquisition
    /// (`stamp_hound_flag`). A decode landing in that window sees
    /// `hound=false` + `partner_freq=Some` and is eligible to record a drift
    /// candidate — this reproduces that window directly (bypassing
    /// `engage_hound`'s single atomic call, exactly as a concurrent decode
    /// would) and proves `stamp_hound_flag` clears the candidate rather than
    /// leaving it stuck forever (the Hound skip in
    /// `maybe_confirm_frequency_drift_at` `continue`s before ever reaching
    /// the in-tolerance reset once `hound=true`, so an uncleared candidate
    /// would otherwise persist in serialized metadata for the QSO's life).
    #[tokio::test]
    async fn stamp_hound_flag_clears_pending_freq_drift_from_the_construction_window() {
        let manager = manager_with_call("K5ARH");

        // Mirrors exactly what `engage_hound` does internally before it stamps
        // `hound=true`: a Manual QSO with `partner_freq` set atomically at
        // construction, `hound` still at its default `false`.
        let qso_id = manager
            .respond_to_cq_with(
                "D2UY".into(),
                700.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(1800.0), // Fox's RX offset
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(!progress.metadata.hound, "sanity: hound not yet stamped");

        // Simulate a decode landing in the window between construction and
        // the hound stamp: still `hound=false`, so this is NOT skipped and
        // records a drift candidate.
        manager
            .maybe_confirm_frequency_drift(
                &MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -12,
                },
                1950.0, // 150 Hz from the Fox's 1800 Hz — beyond ESTABLISHED_FREQ_TOLERANCE_HZ
            )
            .await;
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.pending_freq_drift.is_some(),
            "sanity: the construction window must be able to record a drift candidate"
        );

        // The fix: stamping the hound flag must clear it atomically.
        manager.stamp_hound_flag(qso_id).await;
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(progress.metadata.hound, "hound flag must now be stamped");
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "stamp_hound_flag must clear any drift candidate accumulated in the window, \
             otherwise it can never be cleared again for this Hound QSO's life"
        );
    }

    /// PAN-15 item 3: `tx_separation_warning` boundary behavior — the pure
    /// predicate behind the "relatched partner_freq too close to our own TX
    /// offset" warn.
    #[test]
    fn tx_separation_warning_flags_separations_below_min_tx_separation() {
        assert_eq!(
            QsoManager::tx_separation_warning(2680.0, 2700.0),
            Some(20.0)
        );
        assert_eq!(
            QsoManager::tx_separation_warning(2626.0, 2700.0),
            Some(74.0),
            "1 Hz inside the MIN_TX_SEPARATION_HZ boundary must still warn"
        );
    }

    #[test]
    fn tx_separation_warning_allows_adequate_separations() {
        assert_eq!(
            QsoManager::tx_separation_warning(2625.0, 2700.0),
            None,
            "exactly MIN_TX_SEPARATION_HZ apart is adequate separation"
        );
        assert_eq!(QsoManager::tx_separation_warning(2600.0, 2700.0), None);
    }

    /// PAN-15 item 3 (behavioral): a confirmed relatch that drifts the
    /// partner's RX offset to within `MIN_TX_SEPARATION_HZ` of our own TX
    /// offset still relatches (the ticket's minimum bar is a warn, not a
    /// block) — this proves the warn path doesn't alter the relatch outcome
    /// or panic.
    #[tokio::test]
    async fn relatch_within_min_tx_separation_of_our_own_tx_offset_still_relatches() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_with(
                "K6L".into(),
                2700.0,
                Some(pancetta_core::slot::SlotParity::Even),
                CallInitiation::Manual,
                Some(2931.0),
                false,
            )
            .await
            .unwrap();

        // K6L drifts to 2680 Hz — only 20 Hz from our own 2700 Hz TX offset,
        // well inside MIN_TX_SEPARATION_HZ (75 Hz). Two sightings >=5s apart
        // are required to confirm-and-relatch.
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K6L".into(),
            report: -11,
        };
        manager
            .process_message(report.clone(), "K5ARH K6L -11".into(), 2680.0, Some(-19.0))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        manager
            .process_message(report, "K5ARH K6L -11".into(), 2680.0, Some(-17.0))
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.partner_freq,
            Some(2680.0),
            "the relatch still occurs even when it lands within MIN_TX_SEPARATION_HZ of our TX \
             offset — the ticket's minimum bar is a warn, not a block"
        );
        assert_eq!(
            progress.metadata.frequency, 2700.0,
            "our TX offset itself is never touched by a partner_freq relatch"
        );
    }

    #[tokio::test]
    async fn spoofed_report_ack_does_not_advance_to_completion() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::SendingReport {
            their_callsign: "K9ZZ".into(),
            their_report: Some(-15),
            our_report: -10,
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let spoof = MessageType::ReportAck {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -10,
        };
        let new_state = manager
            .determine_state_transition(
                Uuid::new_v4(),
                &state,
                &spoof,
                None,
                CallInitiation::Auto,
                false,
            )
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::SendingReport { .. }));
    }

    #[tokio::test]
    async fn spoofed_final_confirmation_does_not_complete_qso() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::WaitingForConfirmation {
            their_callsign: "K9ZZ".into(),
            their_report: -15,
            our_report: -10,
            frequency: 1500.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let spoof = MessageType::FinalConfirmation {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
        };
        let new_state = manager
            .determine_state_transition(
                Uuid::new_v4(),
                &state,
                &spoof,
                None,
                CallInitiation::Auto,
                false,
            )
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::WaitingForConfirmation { .. }));
    }

    #[tokio::test]
    async fn legitimate_final_confirmation_completes_qso() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::WaitingForConfirmation {
            their_callsign: "K9ZZ".into(),
            their_report: -15,
            our_report: -10,
            frequency: 1500.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let legit = MessageType::FinalConfirmation {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
        };
        let new_state = manager
            .determine_state_transition(
                Uuid::new_v4(),
                &state,
                &legit,
                None,
                CallInitiation::Auto,
                false,
            )
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::Completed { .. }));
    }
}

#[cfg(test)]
mod reply_emitter_tests {
    //! Auto-sequence reply emitter for MANUAL QSOs.
    //!
    //! Drives a manual QSO through the full inbound exchange and asserts the
    //! outbound `MessageToSend` replies (R-report → RR73 → 73) are emitted,
    //! that the QSO completes + logs, and that autonomous QSOs do NOT
    //! auto-reply.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn manager() -> QsoManager {
        let config = QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        };
        QsoManager::new(config)
    }

    /// Drain currently-buffered events into a Vec.
    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn messages_to_send(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// On-air text the coordinator would generate for an emitted reply.
    fn on_air(msg: &MessageType) -> String {
        crate::utils::generate_ft8_message(msg, OUR).unwrap()
    }

    /// 1. Manual QSO in RespondingToCq + SignalReport → emits ReportAck
    ///    (R+report) and state advances to SendingReport.
    #[tokio::test]
    async fn manual_signal_report_emits_report_ack() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx); // discard the initial CqResponse call

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(
            sends.len(),
            1,
            "expected exactly one reply, got {:?}",
            sends
        );
        match &sends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                // snr -15 → our report -15 (matches SendingReport.our_report).
                assert_eq!(*report, -15);
            }
            other => panic!("expected ReportAck, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH R-15");

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "expected SendingReport, got {:?}",
            progress.state
        );
    }

    /// 2. Manual QSO in SendingReport + ReportAck → emits FinalConfirmation
    ///    (RR73) and state advances to WaitingForConfirmation.
    #[tokio::test]
    async fn manual_report_ack_emits_final_confirmation() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1, "expected one reply, got {:?}", sends);
        match &sends[0] {
            MessageType::FinalConfirmation {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
            }
            other => panic!("expected FinalConfirmation, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH RR73");
    }

    /// 3. Manual QSO in WaitingForConfirmation + FinalConfirmation → emits
    ///    SeventyThree (73), QSO → Completed, and a QsoCompleted event fires
    ///    (so the ADIF logger logs it).
    #[tokio::test]
    async fn manual_final_confirmation_emits_73_and_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        match &sends[0] {
            MessageType::SeventyThree {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
            }
            other => panic!("expected SeventyThree, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

        // QSO completed.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        // QsoCompleted event fired (drives ADIF logging).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event"
        );
    }

    /// FIX 2: Manual QSO in SendingReport + RR73 (FinalConfirmation) → the
    /// DX rogered our R-report directly. We emit our 73, the QSO completes,
    /// and a QsoCompleted event fires (drives ADIF logging). This is the
    /// "never sent 73 / QSO stalled one message short" bug.
    #[tokio::test]
    async fn manual_sending_report_plus_rr73_emits_73_and_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport (DX sent their report; we send R-report).
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        let _ = drain(&mut rx);

        // DX closes directly with RR73 (skips a separate RRR/report-ack).
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
    }

    /// A completed QSO logs the RF frequency (dial + audio offset), not the
    /// bare offset, when a dial-frequency source is shared. Regression for the
    /// ADIF FREQ ~0.001 / BAND 0MHZ bug.
    #[tokio::test]
    async fn completed_metadata_logs_dial_plus_offset() {
        let mut manager = manager();
        manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
        let mut rx = manager.subscribe();
        let _qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let completed = events.iter().find_map(|e| match e {
            QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
            _ => None,
        });
        let metadata = completed.expect("expected a QsoCompleted event");
        // dial 14_074_000 + offset 1500 = 14_075_500 Hz (20m).
        assert_eq!(metadata.frequency, 14_074_000.0 + FREQ);
    }

    /// Without a dial source (e.g. unit tests / no rig), completed metadata
    /// keeps the value it was created with — no spurious offset added.
    #[tokio::test]
    async fn completed_metadata_unchanged_without_dial_source() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let events = drain(&mut rx);
        let metadata = events
            .iter()
            .find_map(|e| match e {
                QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
                _ => None,
            })
            .expect("expected a QsoCompleted event");
        assert_eq!(metadata.frequency, FREQ);
    }

    // --- respond_to_caller: open the exchange at a chosen ResponseStep ----

    use pancetta_core::ResponseStep;

    /// `Grid` opens exactly like the historical manual call: state
    /// `RespondingToCq` and a first message of `CqResponse` carrying our grid.
    #[tokio::test]
    async fn respond_to_caller_grid_matches_legacy_manual() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Grid,
                Some(-12.0),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::CqResponse {
                calling_station,
                responding_station,
                grid,
            } => {
                assert_eq!(calling_station, DX);
                assert_eq!(responding_station, OUR);
                assert_eq!(grid.as_deref(), Some("EM12"));
            }
            other => panic!("expected CqResponse, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
        assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
    }

    /// `Report` opens at state `SendingReport` (their_report None) and a first
    /// message of `SignalReport` carrying the report derived from our SNR.
    #[tokio::test]
    async fn respond_to_caller_report_emits_signal_report() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Report,
                Some(-9.0),
                None,
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::SignalReport {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -9);
            }
            other => panic!("expected SignalReport, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        match progress.state {
            QsoState::SendingReport {
                their_report,
                our_report,
                ..
            } => {
                assert_eq!(their_report, None);
                assert_eq!(our_report, -9);
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// `ReportAck` opens at state `SendingReport` (their_report Some) and a
    /// first message of `ReportAck` (R-report).
    #[tokio::test]
    async fn respond_to_caller_report_ack_emits_report_ack() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::ReportAck,
                Some(-10.0),
                Some(-3),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -10);
            }
            other => panic!("expected ReportAck, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        match progress.state {
            QsoState::SendingReport { their_report, .. } => {
                assert_eq!(their_report, Some(-3));
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// `Rr73` opens at state `WaitingForConfirmation` and a first message of
    /// `FinalConfirmation` (RR73).
    #[tokio::test]
    async fn respond_to_caller_rr73_emits_final_confirmation() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Rr73,
                Some(-5.0),
                Some(-7),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        assert!(
            matches!(sends[0], MessageType::FinalConfirmation { .. }),
            "expected FinalConfirmation, got {:?}",
            sends[0]
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(
            progress.state,
            QsoState::WaitingForConfirmation { .. }
        ));
    }

    /// `SeventyThree` opens directly at `Completed`, emits a `SeventyThree`
    /// first message AND a `QsoCompleted` event so the QSO is logged.
    #[tokio::test]
    async fn respond_to_caller_seventy_three_completes_and_logs() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::Completed { .. }));
    }

    /// FIX B (one-QSO-per-(callsign,band) close idempotency,
    /// docs/qso-tx-deep-review-2026-07-18.md): a station keeps sending us
    /// RR73 (never copied our 73). The operator presses Space again — a
    /// repeat `respond_to_caller(SeventyThree)` for the SAME callsign must
    /// re-emit another 73 (so the DX still gets it) WITHOUT spawning a
    /// sibling `QsoId` or logging a second ADIF entry. Superseded 2026-07-18:
    /// this test used to assert the OPPOSITE — that a repeat press "should
    /// build a fresh QSO" — which was the live double-73 duplicate-log bug
    /// itself (3 SeventyThree presses in 8s produced 4 duplicate ADIF
    /// entries for SM2LIY on-air). `find_recently_completed_manual_qso_for`
    /// now catches the just-completed QSO within its grace window and
    /// re-keys its last frame instead.
    #[tokio::test]
    async fn repeat_respond_to_caller_seventy_three_resends_73() {
        let manager = manager();
        let mut rx = manager.subscribe();

        // First 73.
        let first = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        assert!(matches!(sends[0], MessageType::SeventyThree { .. }));

        // They send us RR73 again; operator presses Space again → second 73.
        let second = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "repeat press within the grace window must resolve to the SAME QSO, \
             never spawn a sibling"
        );
        let events2 = drain(&mut rx);
        let sends2 = messages_to_send(&events2);
        assert_eq!(
            sends2.len(),
            1,
            "repeat Space must re-send a 73, not no-op: {sends2:?}"
        );
        assert!(
            matches!(sends2[0], MessageType::SeventyThree { .. }),
            "expected a second SeventyThree (re-sent), got {:?}",
            sends2[0]
        );

        // Exactly ONE QsoCompleted must have fired across BOTH calls — the
        // second call resolved via resend_last_tx, not a fresh completion.
        assert!(
            !events2
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "second call must NOT emit a new QsoCompleted: {events2:?}"
        );

        // `qsos_by_callsign` must have exactly one entry for DX — no sibling
        // QSO id was ever recorded against this callsign.
        let ids = manager
            .qsos_by_callsign
            .read()
            .await
            .get(&DX.to_uppercase())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            ids.len(),
            1,
            "exactly one QSO id must be mapped to {DX}, got {ids:?}"
        );
    }

    /// FIX B: three RAPID `SeventyThree` presses (mirroring the live incident
    /// — 3 presses in 8 seconds) all resolve to the SAME QSO object;
    /// `call_count` increments once per press (verified via `get_qso`).
    #[tokio::test]
    async fn triple_rapid_seventy_three_press_stays_one_qso_call_count_increments() {
        let manager = manager();
        let mut rx = manager.subscribe();

        let first = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);
        let call_count_after_first = manager.get_qso(first).await.unwrap().metadata.call_count;

        let second = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);
        let call_count_after_second = manager.get_qso(second).await.unwrap().metadata.call_count;

        let third = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);
        let call_count_after_third = manager.get_qso(third).await.unwrap().metadata.call_count;

        assert_eq!(first, second);
        assert_eq!(first, third);
        assert!(
            call_count_after_second > call_count_after_first,
            "call_count must increment on the 2nd press: {call_count_after_first} -> {call_count_after_second}"
        );
        assert!(
            call_count_after_third > call_count_after_second,
            "call_count must increment on the 3rd press: {call_count_after_second} -> {call_count_after_third}"
        );
    }

    /// FIX B: a `SeventyThree` reply made AFTER `COMPLETED_QSO_REWORK_GRACE`
    /// has elapsed since the prior QSO completed must NOT re-key the old
    /// QSO — it opens a genuinely NEW one (the grace window bounds how long
    /// "still trying to close out the same contact" is assumed). Uses the
    /// explicit-`now` testable variant
    /// (`find_recently_completed_manual_qso_for_at`) instead of a real 45s+
    /// `tokio::time::sleep`.
    #[tokio::test]
    async fn seventy_three_after_grace_elapsed_opens_new_qso() {
        let manager = manager();
        let mut rx = manager.subscribe();

        let first = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);

        // Directly exercise the explicit-`now` lookup past the grace window —
        // proves the grace-window boundary itself, independent of
        // `respond_to_caller`'s real-clock dispatch.
        let completed_at = match manager.get_qso(first).await.unwrap().state {
            QsoState::Completed { completed_at, .. } => completed_at,
            other => panic!("expected Completed state, got {other:?}"),
        };
        let just_within = completed_at + COMPLETED_QSO_REWORK_GRACE - Duration::seconds(1);
        let just_past = completed_at + COMPLETED_QSO_REWORK_GRACE + Duration::seconds(1);

        assert_eq!(
            manager
                .find_recently_completed_manual_qso_for_at(
                    DX,
                    FREQ,
                    COMPLETED_QSO_REWORK_GRACE,
                    just_within,
                )
                .await,
            Some(first),
            "within the grace window the completed QSO must still be found"
        );
        assert_eq!(
            manager
                .find_recently_completed_manual_qso_for_at(
                    DX,
                    FREQ,
                    COMPLETED_QSO_REWORK_GRACE,
                    just_past,
                )
                .await,
            None,
            "past the grace window the completed QSO must no longer be found"
        );
    }

    /// FIX B regression: a `Grid` or `Report`/`ReportAck` reply for a station
    /// we JUST completed, within the grace window, is the deliberate
    /// legitimate-rework path (e.g. working the same station again on a
    /// fresh exchange) — it must still open a NEW QSO, not be swallowed into
    /// the just-completed one. Only `Rr73`/`SeventyThree` (close steps) get
    /// the grace-window idempotent-close treatment.
    #[tokio::test]
    async fn grid_or_report_step_within_grace_window_still_opens_new_qso() {
        let manager = manager();
        let mut rx = manager.subscribe();

        let first = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);

        // A fresh Grid-step call (the legitimate-rework case) must build a
        // new QSO, not resolve back to the just-completed one.
        let grid_reopen = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Grid,
                Some(-8.0),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        assert_ne!(
            first, grid_reopen,
            "a Grid-step reply within the grace window must open a NEW QSO \
             (legitimate rework), not re-key the completed one"
        );
        drain(&mut rx);

        // Complete the reopened QSO's grace-window state doesn't matter here;
        // check Report similarly against a fresh completed QSO.
        let second_complete = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
                None,
                false,
            )
            .await
            .unwrap();
        drain(&mut rx);

        let report_reopen = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Report,
                Some(-8.0),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        assert_ne!(
            second_complete, report_reopen,
            "a Report-step reply within the grace window must open a NEW QSO \
             (legitimate rework), not re-key the completed one"
        );
    }

    /// `our_snr_of_them = None` falls back to a sane default report (-15).
    #[tokio::test]
    async fn respond_to_caller_defaults_report_when_no_snr() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Report,
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        match &sends[0] {
            MessageType::SignalReport { report, .. } => assert_eq!(*report, -15),
            other => panic!("expected SignalReport, got {other:?}"),
        }
    }

    /// A full sequence opened at `ReportAck`: we send R-report, the DX answers
    /// RR73, and the QSO completes (and logs) via the normal state machine.
    #[tokio::test]
    async fn respond_to_caller_report_ack_through_rr73_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::ReportAck,
                Some(-10.0),
                Some(-3),
                None, // partner_freq
                false,
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        // DX rogers our R-report with RR73 → we close with 73 and complete.
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-12.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event"
        );
    }

    /// FIX 2: Manual QSO in SendingReport + a bare "73" (DX skipped RR73) →
    /// QSO completes and logs, and we do NOT re-send a 73 (they are done).
    #[tokio::test]
    async fn manual_sending_report_plus_bare_73_completes_without_resend() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} 73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert!(
            sends.is_empty(),
            "bare 73 close must not re-send a 73, got {:?}",
            sends
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
    }

    /// 4. AUTONOMOUS QSO in RespondingToCq + SignalReport → state advances AND
    ///    the forward reply (our R-report / ReportAck) is auto-emitted. This is
    ///    the Phase-5 autonomous auto-completion behavior: forward auto-sequencing
    ///    now fires for Auto QSOs exactly as for Manual (regression handling stays
    ///    Manual-only). (Previously the emitter was Manual-gated and an Auto QSO
    ///    advanced silently — that gate was removed when Phase 5 landed.)
    #[tokio::test]
    async fn auto_qso_advances_and_auto_replies() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.into(), FREQ, None) // CallInitiation::Auto
            .await
            .unwrap();
        let _ = drain(&mut rx); // discard initial CqResponse call

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        // The forward reply IS emitted now (our R-report = ReportAck).
        assert!(
            sends
                .iter()
                .any(|m| matches!(m, MessageType::ReportAck { .. })),
            "autonomous QSO must auto-reply with our R-report (ReportAck), got {:?}",
            sends
        );
        // And the state advanced.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "auto QSO state must advance to SendingReport, got {:?}",
            progress.state
        );
    }

    /// 5. Spurious sender (wrong from/to) is still ignored: no state advance
    ///    and no reply emitted, even for a manual QSO.
    #[tokio::test]
    async fn spurious_sender_ignored_no_reply() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx);

        // Properly-addressed report but from a DIFFERENT callsign.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: "NF4KE".into(),
                    report: -7,
                },
                format!("{} NF4KE -07", OUR),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let sends = messages_to_send(&drain(&mut rx));
        assert!(
            sends.is_empty(),
            "spurious sender must not trigger a reply, got {:?}",
            sends
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "spurious report must not advance state, got {:?}",
            progress.state
        );
    }

    // --- active_tx_offsets (T3) ----------------------------------------------

    /// `active_tx_offsets()` returns the `metadata.frequency` of every
    /// currently-active (non-terminal) QSO. Used by the coordinator to
    /// de-conflict a new QSO's TX offset against live concurrent streams.
    #[tokio::test]
    async fn active_tx_offsets_returns_open_qso_offsets() {
        let manager = manager();
        // Initially empty — no QSOs.
        assert!(
            manager.active_tx_offsets().await.is_empty(),
            "no QSOs → offsets must be empty"
        );

        // Open one QSO at 1234.0 Hz.
        manager
            .respond_to_cq("VK3XYZ".to_string(), 1234.0, None)
            .await
            .expect("open first QSO");

        let offsets = manager.active_tx_offsets().await;
        assert_eq!(offsets.len(), 1, "one active QSO");
        assert!(
            (offsets[0] - 1234.0).abs() < 0.1,
            "offset must match the QSO's audio frequency"
        );

        // Open a second QSO at a different offset.
        manager
            .respond_to_cq("ZL2ABC".to_string(), 1800.0, None)
            .await
            .expect("open second QSO");

        let offsets2 = manager.active_tx_offsets().await;
        assert_eq!(offsets2.len(), 2, "two active QSOs");
        let mut sorted = offsets2.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((sorted[0] - 1234.0).abs() < 0.1);
        assert!((sorted[1] - 1800.0).abs() < 0.1);
    }

    // --- respond_to_cq_with partner_freq param (T3) --------------------------

    /// When `partner_freq = Some(x)` is passed, the created QSO's
    /// `metadata.partner_freq` is `Some(x)` and `metadata.frequency` stays at
    /// the TX offset — so the two fields are independently settable.
    #[tokio::test]
    async fn respond_to_cq_with_partner_freq_some_sets_both_fields() {
        let manager = manager();
        let tx_off = 1234.0_f64;
        let dx_rx = 1500.0_f64;
        let qso_id = manager
            .respond_to_cq_with(
                DX.into(),
                tx_off,
                None,
                CallInitiation::Manual,
                Some(dx_rx), // partner_freq = DX's RX offset
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            (progress.metadata.frequency - tx_off).abs() < 0.1,
            "metadata.frequency must be our TX offset ({tx_off}), got {}",
            progress.metadata.frequency
        );
        assert_eq!(
            progress.metadata.partner_freq,
            Some(dx_rx),
            "metadata.partner_freq must be the DX's RX offset ({dx_rx})"
        );
    }

    /// Regression: `partner_freq = None` (Tx=Rx path) leaves `metadata.partner_freq`
    /// as `None` — behavior identical to pre-T3. The coordinator passes `None`
    /// when Auto mode and no collision.
    #[tokio::test]
    async fn respond_to_cq_with_partner_freq_none_is_txrx_regression() {
        let manager = manager();
        let qso_id = manager
            .respond_to_cq_with(
                DX.into(),
                FREQ,
                None,
                CallInitiation::Manual,
                None, // Tx=Rx regression path — partner_freq must stay None
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.partner_freq, None,
            "partner_freq=None must be preserved on the Tx=Rx (Auto+no-collision) path"
        );
        assert!(
            (progress.metadata.frequency - FREQ).abs() < 0.1,
            "metadata.frequency must equal the passed frequency"
        );
    }

    /// Regression: `respond_to_caller` with `partner_freq = None` (Grid step
    /// routes through `respond_to_cq_with`) — partner_freq must stay None.
    #[tokio::test]
    async fn respond_to_caller_partner_freq_none_is_txrx_regression() {
        let manager = manager();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Grid,
                None,
                None,
                None, // Tx=Rx — partner_freq must stay None
                false,
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            progress.metadata.partner_freq, None,
            "Grid/Tx=Rx path: partner_freq must be None (regression)"
        );
    }

    /// PAN-23: `respond_to_caller` — the TUI Callers "reply" path — must
    /// refuse the literal unresolved-hash placeholder `"<...>"` regardless
    /// of which sequence step the operator (or a queued replay) opens at.
    /// This is the exact failure mode observed on-air (production logs
    /// 2026-08-20..22): the operator selected `"<...>"` from the Callers
    /// panel at step `ReportAck`, pancetta queued/started the QSO, and the
    /// eventual encode attempt failed with "callsign '<...>' cannot be
    /// represented in any FT8 message format". The TUI-side fix (PAN-23,
    /// `app.rs`'s `displayed_callers`) means the operator can no longer
    /// select it, but this backend guard is belt-and-suspenders against any
    /// other path reaching `RespondToCaller` with that target — and it
    /// short-circuits BEFORE any `MessageToSend` is emitted, so no doomed
    /// encode is ever attempted.
    #[tokio::test]
    async fn respond_to_caller_rejects_unresolved_hash_placeholder() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let err = manager
            .respond_to_caller(
                "<...>".to_string(),
                FREQ,
                None,
                ResponseStep::ReportAck,
                Some(-8.0),
                None,
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, QsoManagerError::InvalidCallsign { callsign } if callsign == "<...>"),
            "expected InvalidCallsign for the unresolved hash placeholder, got {err:?}"
        );
        assert!(
            messages_to_send(&drain(&mut rx)).is_empty(),
            "no MessageToSend may be emitted for the unresolved hash placeholder \
             (it can never be encoded into a valid FT8 message)"
        );
    }
}

#[cfg(test)]
mod state_regression_tests {
    //! State-regression intelligence ("back up to where the DX thinks we are").
    //!
    //! When a MANUAL QSO's DX re-sends an EARLIER-stage message — meaning they
    //! never copied our most-recent transmission — the QSO machine regresses to
    //! match the DX and re-sends the appropriate response instead of stalling.
    //!
    //! Re-send duty split:
    //! - REGRESSION 1 (WaitingForConfirmation + repeated report → SendingReport):
    //!   `process_message_for_qso` emits the R-report IMMEDIATELY this slot (via
    //!   the reply emitter's new (WaitingForConfirmation, SignalReport) arm); the
    //!   per-slot `rearm_manual_calls_at` owns subsequent slots.
    //! - REGRESSION 2 (SendingReport + repeated report → stays SendingReport):
    //!   the transition does NOT emit (exchange has no (SendingReport,
    //!   SignalReport) arm); `rearm_manual_calls_at` (FIX 4) owns the R re-send.
    //!   The transition only updates the latched reports. Stamping `last_call_at`
    //!   on the regression gates rearm so the two never double-send in one slot.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn manager_with(max_calls: u32, watchdog_min: u64) -> QsoManager {
        let mut config = QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        };
        config.timeouts.manual_call_max_calls = max_calls;
        config.timeouts.manual_call_watchdog_minutes = watchdog_min;
        QsoManager::new(config)
    }

    fn manager() -> QsoManager {
        manager_with(10, 5)
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Drive a manual QSO to WaitingForConfirmation (CqResponse → R → RR73 to
    /// the DX), returning the qso_id.
    async fn manual_to_waiting_confirmation(manager: &QsoManager) -> QsoId {
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::WaitingForConfirmation { .. }
        ));
        qso_id
    }

    /// REGRESSION 1: WaitingForConfirmation + repeated report → SendingReport,
    /// an R-report is re-emitted, and reports are updated to the newest value.
    #[tokio::test]
    async fn manual_waiting_confirmation_plus_repeated_report_regresses_to_sending_report() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        let _ = drain(&mut rx);

        // DX re-sends their report — with a NEW value — having never copied
        // our RR73. snr -9 → our report -9.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -3,
                },
                format!("{} {} -03", OUR, DX),
                FREQ,
                Some(-9.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        // Regressed two steps back.
        match &progress.state {
            QsoState::SendingReport {
                their_report,
                our_report,
                ..
            } => {
                assert_eq!(*their_report, Some(-3), "their report updated to newest");
                assert_eq!(*our_report, -9, "our report recomputed from newest SNR");
            }
            other => panic!("expected SendingReport, got {:?}", other),
        }

        // R-report re-emitted this slot.
        let emitted = sends(&drain(&mut rx));
        assert_eq!(
            emitted.len(),
            1,
            "expected one R re-send, got {:?}",
            emitted
        );
        match &emitted[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -9);
            }
            other => panic!("expected ReportAck, got {:?}", other),
        }
    }

    /// REGRESSION 2: SendingReport + repeated report → stays SendingReport (no
    /// spurious double-advance); rearm re-sends R (transition itself does not,
    /// avoiding a same-slot double-send).
    #[tokio::test]
    async fn manual_sending_report_repeated_report_stays_and_resends() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));
        let _ = drain(&mut rx);

        // DX re-sends their report (didn't copy our R).
        let result = manager
            .determine_state_transition(
                qso_id,
                &manager.get_qso(qso_id).await.unwrap().state,
                &MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                Some(-15.0),
                CallInitiation::Manual,
                false,
            )
            .await
            .unwrap();
        assert!(
            matches!(result, QsoState::SendingReport { .. }),
            "must stay in SendingReport, got {:?}",
            result
        );

        // Now exercise the full path: it must NOT emit from the transition (no
        // exchange arm); the per-slot rearm owns the R re-send.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let from_transition = sends(&drain(&mut rx));
        assert!(
            from_transition.is_empty(),
            "transition must not re-send (rearm owns it), got {:?}",
            from_transition
        );

        // A slot later, rearm re-sends our R-report (and not before — the
        // regression stamped last_call_at, so no double-send in this slot).
        let last = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        manager
            .rearm_manual_calls_at(last + Duration::seconds(15))
            .await;
        let rearmed = sends(&drain(&mut rx));
        assert_eq!(rearmed.len(), 1, "rearm re-sends R, got {:?}", rearmed);
        assert!(
            matches!(rearmed[0], MessageType::ReportAck { .. }),
            "rearm must re-send ReportAck, got {:?}",
            rearmed[0]
        );
    }

    /// On-air regression (9K2MP, 2026-06-22): once the DX has ENGAGED (we're in
    /// SendingReport — we received their report), the call-count cap must NOT
    /// retire the QSO. Previously a DX that re-requested several times blew past
    /// the 10-call cap and the watchdog retired the QSO one slot before the DX's
    /// closing RR73 — the RR73 then landed on a dead QSO, the contact was lost,
    /// and the operator had to close it by hand. The cap now governs only the
    /// initial-call states (CallingCq/RespondingToCq); an engaged exchange runs
    /// to completion (bounded only by the time watchdog).
    #[tokio::test]
    async fn engaged_sending_report_survives_call_cap_and_completes_on_rr73() {
        let manager = manager_with(3, 60); // cap 3, generous 60-min time window
        let qso_id = manual_to_waiting_confirmation(&manager).await;

        // DX re-requests its report many times → regress to SendingReport and
        // blow well past the call cap.
        for _ in 0..6 {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: OUR.into(),
                        from_station: DX.into(),
                        report: -7,
                    },
                    format!("{} {} -07", OUR, DX),
                    FREQ,
                    Some(-15.0),
                )
                .await
                .unwrap();
        }
        let prog = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(prog.state, QsoState::SendingReport { .. }),
            "should be engaged (SendingReport), got {:?}",
            prog.state
        );
        assert!(
            prog.metadata.call_count >= 3,
            "call cap should be exceeded, got {}",
            prog.metadata.call_count
        );
        let first = prog.metadata.first_call_at.unwrap();

        // Within the time window: the cap must NOT retire an engaged QSO.
        manager
            .check_timeouts_at(first + Duration::minutes(1))
            .await;
        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "engaged SendingReport must NOT be retired by the call cap"
        );

        // The DX's closing RR73 now completes the QSO (it was alive to copy it).
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager.get_qso(qso_id).await.unwrap().state,
                QsoState::Completed { .. }
            ),
            "RR73 must complete the engaged QSO instead of being dropped"
        );
    }

    /// The TIME watchdog still bounds a SendingReport that never closes (DX
    /// re-requests forever): removing the call cap there must not make it
    /// immortal.
    #[tokio::test]
    async fn engaged_sending_report_still_retires_on_time_watchdog() {
        let manager = manager_with(3, 5); // cap 3, 5-min time watchdog
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        for _ in 0..4 {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: OUR.into(),
                        from_station: DX.into(),
                        report: -7,
                    },
                    format!("{} {} -07", OUR, DX),
                    FREQ,
                    Some(-15.0),
                )
                .await
                .unwrap();
        }
        let first = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .first_call_at
            .unwrap();
        // Past the 5-min watchdog → retired even though "engaged".
        manager
            .check_timeouts_at(first + Duration::minutes(6))
            .await;
        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "time watchdog must still bound a never-closing SendingReport"
        );
    }

    /// Our SENT report value stays latched across DX re-requests — it must not
    /// jitter with per-decode SNR noise (observed on-air: R-7 → R-9 → R-6).
    #[tokio::test]
    async fn our_report_value_latched_across_repeated_dx_reports() {
        let manager = manager_with(20, 60);
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // First DX report at SNR -15 → we latch our_report.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let first_report = match manager.get_qso(qso_id).await.unwrap().state {
            QsoState::SendingReport { our_report, .. } => our_report,
            other => panic!("expected SendingReport, got {other:?}"),
        };
        // DX re-requests; this decode has a very different SNR. our_report must
        // NOT change.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-2.0),
            )
            .await
            .unwrap();
        let second_report = match manager.get_qso(qso_id).await.unwrap().state {
            QsoState::SendingReport { our_report, .. } => our_report,
            other => panic!("expected SendingReport, got {other:?}"),
        };
        assert_eq!(
            first_report, second_report,
            "our sent report must stay latched across DX re-requests"
        );
    }

    /// A spurious sender (correct to:, wrong from:) does NOT trigger regression.
    #[tokio::test]
    async fn regression_requires_matching_sender() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        let before = manager.get_qso(qso_id).await.unwrap().metadata.call_count;
        let _ = drain(&mut rx);

        // Properly-addressed report but from a DIFFERENT callsign.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: "NF4KE".into(),
                    report: -7,
                },
                format!("{} NF4KE -07", OUR),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        // No regression: still WaitingForConfirmation, no re-send, no count bump.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "spurious sender must not regress, got {:?}",
            progress.state
        );
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "spurious sender must not re-send"
        );
        assert_eq!(
            progress.metadata.call_count, before,
            "spurious sender must not count against cap"
        );
    }

    /// An AUTO-initiated QSO with a repeated earlier-stage message does NOT
    /// regress or auto-resend (manual-only gate).
    #[tokio::test]
    async fn auto_qso_does_not_regress() {
        let manager = manager();
        let mut rx = manager.subscribe();
        // Build an AUTO QSO and drive it forward to WaitingForConfirmation. The
        // auto path does not auto-reply, so we drive the state directly via
        // process_message (state machine advances regardless of mode).
        let qso_id = manager.respond_to_cq(DX.into(), FREQ, None).await.unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::WaitingForConfirmation { .. }
        ));
        let _ = drain(&mut rx);

        // DX repeats their report. Auto QSO must NOT regress and must NOT
        // auto-resend.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "auto QSO must NOT regress, got {:?}",
            progress.state
        );
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "auto QSO must not auto-resend on regression"
        );
    }
}

#[cfg(test)]
mod sm_f4_waiting_for_report_resend_tests {
    //! SM-F4 (Batch 4): the CQer role previously had NO resilience to a
    //! caller repeating their grid (CqResponse) after missing our
    //! SignalReport in `WaitingForReport` — the (WaitingForReport,
    //! CqResponse) pair fell through to a no-op, so a caller who never copied
    //! our report caused the QSO to silently die at the 30s report_timeout.
    //!
    //! This module tests the new regression arm (stay in WaitingForReport,
    //! re-send the SAME latched `our_report` — no SNR jitter), its sender
    //! verification, and the watchdog-exemption change that lets a CQer in
    //! WaitingForReport survive the generic 30s timeout under the manual
    //! keep-call watchdog (mirroring SendingReport's existing exemption).
    use super::*;

    const OUR: &str = "W1ABC"; // test_config()'s our_callsign
    const CALLER: &str = "K1DEF";
    const FREQ: f64 = 14074000.0;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: OUR.to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Drive a Manual CQ to `WaitingForReport` (opening CQ, then a caller's
    /// grid-bearing CqResponse), returning the qso_id and the `our_report`
    /// value latched on that transition.
    async fn manual_cq_to_waiting_for_report(manager: &QsoManager, snr: f32) -> (QsoId, i8) {
        let qso_id = manager.start_cq_manual(FREQ, None, false).await.unwrap();
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: OUR.to_string(),
                    responding_station: CALLER.to_string(),
                    grid: Some("FN31".to_string()),
                },
                format!("{OUR} {CALLER} FN31"),
                FREQ,
                Some(snr),
            )
            .await
            .unwrap();
        let our_report = match manager.get_qso(qso_id).await.unwrap().state {
            QsoState::WaitingForReport { our_report, .. } => our_report,
            other => panic!("expected WaitingForReport, got {other:?}"),
        };
        (qso_id, our_report)
    }

    /// A repeated CqResponse from the SAME caller stays in WaitingForReport
    /// (does not advance, does not fail) and the rearm re-sends the SAME
    /// `our_report` value both times — proving no SNR-jitter across repeats,
    /// mirroring REGRESSION 2's anti-jitter contract for the CQer role.
    #[tokio::test]
    async fn repeated_cq_response_stays_and_resends_identical_report() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let (qso_id, first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;
        let _ = drain(&mut rx);

        // Caller repeats their grid (never copied our report) — a fresh
        // decode with a DIFFERENT SNR, to prove our_report does NOT jitter.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: OUR.to_string(),
                    responding_station: CALLER.to_string(),
                    grid: Some("FN31".to_string()),
                },
                format!("{OUR} {CALLER} FN31"),
                FREQ,
                Some(-2.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        let (state_report, their_grid) = match &progress.state {
            QsoState::WaitingForReport {
                our_report,
                their_grid,
                ..
            } => (*our_report, their_grid.clone()),
            other => panic!("expected to stay in WaitingForReport, got {other:?}"),
        };
        assert_eq!(
            state_report, first_report,
            "our_report must stay latched across a repeated CqResponse (no SNR jitter)"
        );
        assert_eq!(their_grid, Some("FN31".to_string()));

        // The transition itself must not emit (exchange.rs has no
        // (WaitingForReport, CqResponse) arm) — the rearm owns the re-send.
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "transition must not re-send directly; rearm owns it"
        );

        // A slot later, the rearm re-sends our SignalReport with the
        // IDENTICAL report value.
        let last_call_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        manager
            .rearm_manual_calls_at(last_call_at + Duration::seconds(15))
            .await;
        let rearmed = sends(&drain(&mut rx));
        assert_eq!(
            rearmed.len(),
            1,
            "expected one rearm re-send, got {rearmed:?}"
        );
        match &rearmed[0] {
            MessageType::SignalReport {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, CALLER);
                assert_eq!(from_station, OUR);
                assert_eq!(
                    *report, first_report,
                    "rearm must re-send the SAME report value latched at the first send"
                );
            }
            other => panic!("expected SignalReport, got {other:?}"),
        }

        // Drive a SECOND repeat + rearm cycle and confirm the value is STILL
        // byte-identical across both repeats (not just the first one).
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: OUR.to_string(),
                    responding_station: CALLER.to_string(),
                    grid: Some("FN31".to_string()),
                },
                format!("{OUR} {CALLER} FN31"),
                FREQ,
                Some(12.0),
            )
            .await
            .unwrap();
        let last_call_at2 = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        manager
            .rearm_manual_calls_at(last_call_at2 + Duration::seconds(15))
            .await;
        let rearmed2 = sends(&drain(&mut rx));
        assert_eq!(rearmed2.len(), 1);
        match &rearmed2[0] {
            MessageType::SignalReport { report, .. } => {
                assert_eq!(
                    *report, first_report,
                    "report must remain byte-identical across BOTH repeats, no jitter"
                );
            }
            other => panic!("expected SignalReport, got {other:?}"),
        }
    }

    /// A repeated CqResponse from a DIFFERENT (non-partner) station is
    /// rejected via the qso.security path and does NOT trigger the stay-arm.
    #[tokio::test]
    async fn repeated_cq_response_from_non_partner_is_rejected() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let (qso_id, first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;
        let _ = drain(&mut rx);

        // A DIFFERENT station answers our CQ with the SAME slot pattern —
        // must not be treated as our caller's repeat.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: OUR.to_string(),
                    responding_station: "NF4KE".to_string(),
                    grid: Some("EM12".to_string()),
                },
                format!("{OUR} NF4KE EM12"),
                FREQ,
                Some(5.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        match &progress.state {
            QsoState::WaitingForReport {
                their_callsign,
                our_report,
                ..
            } => {
                assert_eq!(their_callsign, CALLER, "partner must not change");
                assert_eq!(
                    *our_report, first_report,
                    "our_report must not change from a spurious sender"
                );
            }
            other => panic!("expected to remain in WaitingForReport, got {other:?}"),
        }
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "spurious sender must not trigger a re-send"
        );
    }

    /// A Manual CQer in WaitingForReport is NOT retired by the generic 30s
    /// report_timeout — it survives well past 30s under the manual
    /// watchdog's longer bound (mirrors the 9K2MP-style SendingReport
    /// watchdog-exemption test) but IS eventually retired by the 5-min /
    /// max-calls watchdog bound.
    #[tokio::test]
    async fn manual_waiting_for_report_survives_generic_timeout_but_not_watchdog() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 20;
        config.timeouts.manual_call_watchdog_minutes = 5;
        let manager = QsoManager::new(config);
        let (qso_id, _first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;

        // Well past the generic 30s report_timeout, but well within the 5-min
        // manual watchdog — must still be alive.
        manager
            .check_timeouts_at(Utc::now() + Duration::seconds(90))
            .await;
        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "a Manual CQer in WaitingForReport must survive past the generic 30s report_timeout"
        );

        // Past the 5-min watchdog bound — must now be retired (not immortal).
        let first_call_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .first_call_at
            .unwrap();
        manager
            .check_timeouts_at(first_call_at + Duration::minutes(6))
            .await;
        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "the manual watchdog must still retire a WaitingForReport CQer eventually"
        );
    }
}

#[cfg(test)]
mod sm_f6_auto_bounded_resend_tests {
    //! SM-F6 (Batch 4): bounded re-send/regression resilience for AUTO QSOs.
    //!
    //! `rearm_manual_calls_at` previously skipped every Auto QSO outright
    //! (`if initiated_by != Manual { continue; }`), so an autonomous pounce
    //! whose reply the DX missed got ZERO re-sends and silently died at the
    //! existing 30s `report_timeout`. This module proves the new bounded
    //! resend (`AUTO_RESEND_MAX_CALLS = 2`: the opening send plus exactly one
    //! resend) lands within that unchanged 30s window and does NOT make the
    //! QSO immortal.
    use super::*;

    const OUR: &str = "W1ABC"; // test_config()'s our_callsign
    const DX: &str = "K1DEF";
    const FREQ: f64 = 14074000.0;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: OUR.to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// An Auto QSO in `RespondingToCq` that never gets a DX reply receives
    /// exactly ONE bounded resend around the ~15s mark (the SLOT_SECONDS
    /// cadence), and is still retired at the (unmodified) 30s report_timeout
    /// — not immortal.
    #[tokio::test]
    async fn auto_responding_to_cq_gets_one_bounded_resend_then_retires_at_30s() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // At +15s (one FT8 slot): the bounded Auto resend fires.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        let resent = sends(&drain(&mut rx));
        assert_eq!(
            resent.len(),
            1,
            "expected exactly one bounded Auto resend at +15s, got {resent:?}"
        );
        assert!(
            matches!(resent[0], MessageType::CqResponse { .. }),
            "Auto resend in RespondingToCq must re-send our CqResponse (call), got {:?}",
            resent[0]
        );

        // At +30s (another slot): the cap (AUTO_RESEND_MAX_CALLS = 2) must
        // now block any further resend — call_count is already 2.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(30))
            .await;
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "Auto resend must be capped — no second resend"
        );

        // Still alive at +30s exactly (state_duration comparison is strict
        // `>`), then check_timeouts_at retires it once duration exceeds the
        // 30s report_timeout — not immortal.
        assert!(
            manager.get_qso(qso_id).await.is_ok(),
            "QSO should still be alive exactly at the 30s boundary"
        );
        manager
            .check_timeouts_at(opened_at + Duration::seconds(31))
            .await;
        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "an unanswered Auto pounce must still retire at the unmodified 30s report_timeout"
        );
    }

    /// The bounded Auto resend also covers `SendingReport` (the DX never
    /// rogers our R-report): one resend, then the same 30s bound applies.
    #[tokio::test]
    async fn auto_sending_report_gets_one_bounded_resend_then_retires_at_30s() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        // DX sends their report → advances RespondingToCq -> SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.to_string(),
                    from_station: DX.to_string(),
                    report: -9,
                },
                format!("{OUR} {DX} -09"),
                FREQ,
                Some(-11.0),
            )
            .await
            .unwrap();
        let entered_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));
        let _ = drain(&mut rx);

        // +15s: one bounded resend of our R-report (ReportAck) — the DX
        // already gave us their plain report to get here (their_report is
        // Some on this rung), so the rearm arm re-sends ReportAck, not a
        // plain SignalReport (see the SendingReport rearm arm's own comment).
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(15))
            .await;
        let resent = sends(&drain(&mut rx));
        assert_eq!(
            resent.len(),
            1,
            "expected one bounded resend, got {resent:?}"
        );
        assert!(matches!(resent[0], MessageType::ReportAck { .. }));

        // +30s: capped, no further resend.
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(30))
            .await;
        assert!(sends(&drain(&mut rx)).is_empty(), "resend must be capped");

        // Still bounded by the unmodified 30s report_timeout. `entered_at` is
        // `last_call_at` stamped when the QSO OPENED (Auto forward advances
        // do not stamp `last_call_at` — see `process_message_for_qso`'s
        // Manual-only gate), which lands a fraction of a millisecond BEFORE
        // the SendingReport state's own `started_at` (stamped moments later
        // by the SignalReport transition). `chrono::Duration::num_seconds`
        // truncates towards zero, so a tight +31s margin can read as 30
        // (e.g. 30.9996s) and spuriously miss the boundary — use a
        // comfortable +33s margin instead.
        manager
            .check_timeouts_at(entered_at + Duration::seconds(33))
            .await;
        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "an Auto SendingReport that never closes must still retire at 30s"
        );
    }
}

#[cfg(test)]
mod sm_f5_qso_failed_event_tests {
    //! SM-F5 (Batch 4): `QsoEvent::QsoFailed` was never emitted by `cancel_qso`,
    //! `check_timeouts_at`'s retirement loop, or `supersede_active_qsos_for` —
    //! only `StateChanged → Failed`. The coordinator's priority-scoring
    //! failure backoff (`record_failure`) subscribes to `QsoFailed`
    //! specifically, so this dropped the backoff signal entirely for every
    //! failure path. This module proves all three producer sites now emit
    //! `QsoFailed` with the correct reason and non-empty metadata.
    use super::*;

    const OUR: &str = "W1ABC"; // test_config()'s our_callsign
    const DX: &str = "K1DEF";
    const FREQ: f64 = 14074000.0;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: OUR.to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn qso_failed_events(events: &[QsoEvent]) -> Vec<(QsoId, QsoFailureReason, QsoMetadata)> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::QsoFailed {
                    qso_id,
                    reason,
                    metadata,
                    ..
                } => Some((*qso_id, reason.clone(), metadata.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn cancel_qso_emits_qso_failed_with_user_cancelled() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager.cancel_qso(qso_id).await.unwrap();

        let events = drain(&mut rx);
        // Both surfaces must fire: StateChanged -> Failed (existing) AND the
        // revived QsoFailed (SM-F5).
        assert!(
            events.iter().any(|e| matches!(
                e,
                QsoEvent::StateChanged {
                    new_state: QsoState::Failed { .. },
                    ..
                }
            )),
            "expected a StateChanged -> Failed, got {events:?}"
        );
        let failed = qso_failed_events(&events);
        assert_eq!(
            failed.len(),
            1,
            "expected exactly one QsoFailed, got {failed:?}"
        );
        let (failed_id, reason, metadata) = &failed[0];
        assert_eq!(*failed_id, qso_id);
        assert_eq!(*reason, QsoFailureReason::UserCancelled);
        assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
    }

    #[tokio::test]
    async fn fail_qso_emits_qso_failed_with_the_given_reason() {
        // Task 6 (task-supervision): the coordinator's task supervisor calls
        // `fail_qso` (not `cancel_qso`, which is hardcoded to
        // UserCancelled) to surface a QSO dropped by a Qso-component
        // restart as SupervisorRestart specifically.
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .fail_qso(qso_id, QsoFailureReason::SupervisorRestart)
            .await
            .unwrap();

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                QsoEvent::StateChanged {
                    new_state: QsoState::Failed { .. },
                    ..
                }
            )),
            "expected a StateChanged -> Failed, got {events:?}"
        );
        let failed = qso_failed_events(&events);
        assert_eq!(
            failed.len(),
            1,
            "expected exactly one QsoFailed, got {failed:?}"
        );
        let (failed_id, reason, metadata) = &failed[0];
        assert_eq!(*failed_id, qso_id);
        assert_eq!(*reason, QsoFailureReason::SupervisorRestart);
        assert_eq!(metadata.their_callsign.as_deref(), Some(DX));

        // The QSO must actually leave the active map -- otherwise it would
        // both show as "dropped" in Recent-QSOs AND still be sitting in
        // get_active_qsos() for a subsequent supervisor pass to double-fail.
        assert!(manager.get_qso(qso_id).await.is_err());
    }

    #[tokio::test]
    async fn check_timeouts_at_retirement_emits_qso_failed_with_timeout() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // Past the 30s report_timeout, past the one bounded Auto resend cap
        // too, so this retires cleanly via the timeout path.
        manager
            .check_timeouts_at(opened_at + Duration::seconds(45))
            .await;

        let events = drain(&mut rx);
        let failed = qso_failed_events(&events);
        assert_eq!(
            failed.len(),
            1,
            "expected exactly one QsoFailed, got {failed:?}"
        );
        let (failed_id, reason, metadata) = &failed[0];
        assert_eq!(*failed_id, qso_id);
        assert_eq!(*reason, QsoFailureReason::Timeout);
        assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
    }

    /// Layer 2 timeline persistence: `check_timeouts_at`'s retirement loop is
    /// the single most common "QSO leaves the active map" site (every
    /// watchdog/timeout retirement runs through it) and used to just drop
    /// `progress` — state_history and messages included — on the floor after
    /// cloning out `metadata`. The `QsoFailed` event it emits must now carry
    /// the real timeline instead.
    #[tokio::test]
    async fn check_timeouts_at_retirement_qso_failed_event_carries_timeline() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();

        manager
            .check_timeouts_at(opened_at + Duration::seconds(45))
            .await;

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let (last_state, state_history, messages) = events
            .into_iter()
            .find_map(|e| match e {
                QsoEvent::QsoFailed {
                    qso_id: id,
                    last_state,
                    state_history,
                    messages,
                    ..
                } if id == qso_id => Some((last_state, state_history, messages)),
                _ => None,
            })
            .expect("expected a QsoFailed event for this qso_id");

        // `respond_to_cq` records the opening CqResponse reply as a Sent
        // message at construction, so this must be non-empty — the old
        // hard-coded discard would have made this an empty vec regardless
        // of what the live QSO actually held.
        assert!(
            !messages.is_empty(),
            "QsoFailed must carry the QSO's real message history, not an empty vec"
        );
        // This particular QSO times out before any `process_message`-driven
        // forward advance, so 0 transitions is the CORRECT real value here
        // (not a sign of discard) — the type-check that `state_history`
        // exists on the event at all is what the earlier compile-time
        // wiring enforces.
        assert_eq!(state_history, Vec::<StateTransition>::new());
        assert!(
            matches!(last_state, QsoState::RespondingToCq { .. }),
            "QsoFailed must preserve the operational state even with empty history"
        );
    }

    #[tokio::test]
    async fn supersede_active_qsos_for_emits_qso_failed_with_superseded() {
        let manager = QsoManager::new(test_config());
        let mut rx = manager.subscribe();
        let qso_id_1 = manager
            .respond_to_cq_manual(DX.to_string(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx);

        // Drive the producer directly (FIX 3's own entry point) — a fresh
        // `respond_to_cq_manual` for the SAME (callsign, band) instead hits the
        // idempotent re-call branch (`find_active_manual_qso_for`) while the
        // first QSO is still active, which is a separate code path from the
        // genuine supersede this test targets.
        manager.supersede_active_qsos_for(DX, FREQ).await;

        let events = drain(&mut rx);
        let failed = qso_failed_events(&events);
        assert_eq!(
            failed.len(),
            1,
            "expected exactly one QsoFailed for the superseded QSO, got {failed:?}"
        );
        let (failed_id, reason, metadata) = &failed[0];
        assert_eq!(
            *failed_id, qso_id_1,
            "the OLDER qso must be the one superseded"
        );
        assert_eq!(*reason, QsoFailureReason::Superseded);
        assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
    }
}

#[cfg(test)]
mod tx_frequency_hold_tests {
    //! Mid-QSO TX-frequency hold (operator request): we hold the QSO's
    //! latched TX audio offset for the whole exchange so long as it is
    //! "working" (the DX keeps advancing) — offset is otherwise irrelevant
    //! to whether the DX hears us, since FT8 receivers decode the whole
    //! passband. What happens once it *stops* working (the DX stalls) is
    //! `pan72_stall_detection_tests`, not this module — this module
    //! previously also covered the fixed +300Hz repeat-frame "stuck-DX hop"
    //! escape mechanism, removed by PAN-72 in favor of the silence-driven
    //! stall detector there.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn manager() -> QsoManager {
        QsoManager::new(QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        })
    }

    async fn freq_of(manager: &QsoManager, id: QsoId) -> f64 {
        manager.get_qso(id).await.unwrap().metadata.frequency
    }

    async fn send_report(manager: &QsoManager, report: i8, text: &str) {
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report,
                },
                text.to_string(),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
    }

    /// A normal advancing exchange holds the TX frequency unchanged.
    #[tokio::test]
    async fn frequency_held_across_advancing_exchange() {
        let manager = manager();
        let id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        assert_eq!(freq_of(&manager, id).await, FREQ);

        // DX report (advance), then their R-report ack (advance), then 73.
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
        assert_eq!(freq_of(&manager, id).await, FREQ, "held after report");
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{OUR} {DX} R-07"),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert_eq!(freq_of(&manager, id).await, FREQ, "held through the QSO");
    }

    #[test]
    fn effective_tx_dial_simplex_and_split() {
        // Simplex: split == 0 → use RX dial.
        assert_eq!(super::effective_tx_dial(14_074_000, 0), 14_074_000);
        // Split: nonzero split → use split TX dial.
        assert_eq!(super::effective_tx_dial(14_074_000, 14_090_000), 14_090_000);
    }

    #[test]
    fn split_source_overrides_rx_dial_for_stamp() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let rx = std::sync::Arc::new(AtomicU64::new(14_074_000));
        let split = std::sync::Arc::new(AtomicU64::new(14_090_000));
        let dial =
            super::effective_tx_dial(rx.load(Ordering::Relaxed), split.load(Ordering::Relaxed));
        assert_eq!(dial, 14_090_000);
        split.store(0, Ordering::Relaxed);
        let dial =
            super::effective_tx_dial(rx.load(Ordering::Relaxed), split.load(Ordering::Relaxed));
        assert_eq!(dial, 14_074_000);
    }
}

#[cfg(test)]
mod pan72_stall_detection_tests {
    //! PAN-72: silence-driven stall detection + Switch/Revert event emission.
    //!
    //! Replaces the old identical/differing-frame `DX_STUCK_REPEAT_THRESHOLD`
    //! hop (see the deleted `stuck_dx_tests` cases) with a single counter
    //! (`QsoMetadata::stall_cycles`) incremented solely by
    //! `QsoManager::rearm_manual_calls_at` on each real per-slot re-send —
    //! i.e. driven by *silence* (the DX not responding this slot), not by
    //! inspecting incoming frame content. A genuine forward advance resets
    //! the counter and records the current offset as
    //! `QsoMetadata::last_known_good_offset_hz`. When the counter trips
    //! `TimeoutConfig::qso_stall_switch_after` (Auto TX-freq mode only), the
    //! manager emits `QsoEvent::TxOffsetActionNeeded`: `Switch` if we're
    //! still on the known-good offset (or none is recorded yet), `Revert` if
    //! we're on some other (previously-switched) offset.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }

    /// Manager in Auto TX-freq mode (autonomous offset actions enabled).
    fn manager_auto(config: QsoManagerConfig) -> QsoManager {
        let mut m = QsoManager::new(config);
        m.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        m
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// DX sends us a plain signal report — a genuine forward advance from
    /// `RespondingToCq` to `SendingReport` (their_report: None rung).
    async fn send_report(manager: &QsoManager, report: i8, text: &str) {
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report,
                },
                text.to_string(),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
    }

    /// Two silent rearm cycles (threshold = 2) on an Auto, tx_freq_mode=Auto
    /// QSO must increment `stall_cycles` and, on the threshold-hitting cycle,
    /// emit `TxOffsetActionNeeded::Switch` (no known-good offset recorded
    /// yet, so we're "on" it vacuously).
    ///
    /// Uses a Manual-initiated QSO (`respond_to_cq_manual`) so the rearm
    /// loop's `manual_call_max_calls` bound (25, not the 2-call
    /// `AUTO_RESEND_MAX_CALLS` cap for `CallInitiation::Auto` QSOs) doesn't
    /// retire the QSO's resend eligibility before two rearm cycles complete
    /// — "Auto" here refers to `TxFreqMode`, not `CallInitiation`, mirroring
    /// the old (deleted) `stuck_dx_tests::manager_auto` +
    /// `respond_to_cq_manual` combination.
    #[tokio::test]
    async fn silence_increments_stall_cycles_via_rearm_and_emits_switch_at_threshold() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // First rearm tick: one slot after the initial call, no DX response.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            1,
            "one silent rearm cycle must bump stall_cycles to 1"
        );
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
            "no action expected before the threshold, got {events:?}"
        );

        // Second rearm tick: threshold (2) hit -> Switch.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(30))
            .await;

        let mut saw_switch = false;
        for event in drain(&mut rx) {
            if let QsoEvent::TxOffsetActionNeeded {
                qso_id: id,
                action: OffsetAction::Switch { avoid_hz },
                ..
            } = event
            {
                assert_eq!(id, qso_id);
                assert!(
                    (avoid_hz - FREQ).abs() < f64::EPSILON,
                    "Switch must avoid the current (held) offset"
                );
                saw_switch = true;
            }
        }
        assert!(
            saw_switch,
            "expected a Switch action after 2 silent rearm cycles"
        );
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            0,
            "stall_cycles must reset once the action is emitted"
        );
    }

    /// PAN-72 (Codex round 2 on PR #350, finding 5): while `TxPolicy` is
    /// `Disabled` the coordinator's hard TX mute blocks every frame this loop
    /// re-emits. The keep-calling loop still re-arms each slot, but nothing
    /// goes on the air — so the DX's continued silence is evidence of nothing,
    /// and counting it used to walk an established QSO off its known-good
    /// offset after four muted cycles without a single transmission.
    #[tokio::test]
    async fn a_muted_tx_path_does_not_count_rearm_cycles_as_stalls() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let mut manager = manager_auto(config);
        let policy = Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxPolicy::Disabled.as_u8(),
        ));
        manager.set_tx_policy_source(Arc::clone(&policy));
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // Four muted rearm cycles — twice the switch threshold.
        for slot in 1..=4 {
            manager
                .rearm_manual_calls_at(opened_at + Duration::seconds(15 * slot))
                .await;
        }

        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            0,
            "a rearm cycle the TX mute swallowed must not count as a silent \
             on-air cycle"
        );
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
            "no offset action may be raised while TX is muted, got {events:?}"
        );
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.frequency,
            FREQ,
            "the QSO must still be on its original offset"
        );

        // Re-enabling TX resumes normal stall accounting from where it was.
        policy.store(
            pancetta_core::TxPolicy::Full.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15 * 5))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            1,
            "the first genuinely transmitted cycle after the mute lifts counts"
        );
    }

    /// A genuine forward advance resets `stall_cycles` to 0 and records the
    /// QSO's current offset as `last_known_good_offset_hz`.
    #[tokio::test]
    async fn forward_advance_resets_stall_cycles_and_records_known_good_offset() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();

        // One silent rearm cycle: stall_cycles -> 1 (below threshold).
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            1
        );

        // DX genuinely advances the QSO: RespondingToCq -> SendingReport.
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.stall_cycles, 0,
            "forward advance must reset stall_cycles"
        );
        assert_eq!(
            after.metadata.last_known_good_offset_hz,
            Some(after.metadata.frequency),
            "forward advance must record the current offset as known-good"
        );
    }

    /// A QSO that stalls a second time on an offset it was already switched
    /// to (i.e. currently NOT on its recorded known-good offset) must emit
    /// `Revert { target_hz }` back to the known-good offset, not another
    /// `Switch`.
    #[tokio::test]
    async fn second_stall_on_switched_offset_reverts_to_known_good() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();

        // Advance once to establish last_known_good_offset_hz = Some(FREQ).
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
        let after_advance = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after_advance.metadata.stall_cycles, 0);
        assert_eq!(after_advance.metadata.last_known_good_offset_hz, Some(FREQ));
        let entered_at = after_advance.metadata.last_call_at.unwrap();
        let _ = drain(&mut rx);

        // Two silent rearm cycles on the (still known-good) offset -> Switch.
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(15))
            .await;
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(30))
            .await;
        let first_action = drain(&mut rx).into_iter().find_map(|e| match e {
            QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
            _ => None,
        });
        assert_eq!(
            first_action,
            Some(OffsetAction::Switch { avoid_hz: FREQ }),
            "first stall (still on known-good offset) must Switch"
        );

        // Resolve the Switch the way the coordinator's drain really does —
        // through `apply_tx_offset_switch`, the sole external mutator — so
        // the ping-pong below is driven by the production commit path rather
        // than a hand-poked metadata field. `FREQ + 300` is comfortably
        // inside TX_OFFSET_MIN_HZ..=TX_OFFSET_MAX_HZ, so the clamp is a no-op
        // here.
        let new_freq = FREQ + 300.0;
        let applied = manager
            .apply_tx_offset_switch(qso_id, new_freq, None)
            .await
            .expect("committing the resolved Switch");
        assert_eq!(applied, new_freq);

        // Two more silent rearm cycles on the NEW (non-known-good) offset ->
        // Revert back to the known-good offset, not another Switch.
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(45))
            .await;
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(60))
            .await;
        let second_action = drain(&mut rx).into_iter().find_map(|e| match e {
            QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
            _ => None,
        });
        assert_eq!(
            second_action,
            Some(OffsetAction::Revert { target_hz: FREQ }),
            "second stall (now off the known-good offset) must Revert, not Switch"
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 2): the CQer's real
    /// `CallingCq -> WaitingForReport` advance — a caller finally answering
    /// our CQ — must reset `stall_cycles` and record the offset as
    /// known-good, exactly like the Caller flow's
    /// `RespondingToCq -> SendingReport`.
    ///
    /// Before the fix `dx_frame_advanced` was computed from `ladder_rank`,
    /// which ranks neither `CallingCq` nor `WaitingForReport`, so the streak
    /// accumulated over unanswered CQs carried straight into the established
    /// exchange.
    #[tokio::test]
    async fn cqer_flow_advance_resets_stall_cycles_and_records_known_good_offset() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 4;
        let manager = manager_auto(config);
        let qso_id = manager.start_cq_manual(FREQ, None, false).await.unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();

        // Three unanswered CQ slots: stall_cycles -> 3 (still below the
        // default-shaped threshold of 4).
        for i in 1..=3i64 {
            manager
                .rearm_manual_calls_at(opened_at + Duration::seconds(15 * i))
                .await;
        }
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
            3,
            "precondition: three unanswered CQ slots accumulated a streak"
        );

        // A caller finally answers: CallingCq -> WaitingForReport. This is a
        // genuine forward advance and must clear the accumulated streak.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: OUR.into(),
                    responding_station: DX.into(),
                    grid: Some("FN31".into()),
                },
                format!("{OUR} {DX} FN31"),
                FREQ,
                Some(-9.0),
            )
            .await
            .unwrap();

        let after = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(after.state, QsoState::WaitingForReport { .. }),
            "precondition: the answer must advance the CQer to WaitingForReport, got {:?}",
            after.state
        );
        assert_eq!(
            after.metadata.stall_cycles, 0,
            "a caller answering our CQ is forward progress — the unanswered-CQ \
             streak must not carry into the established exchange"
        );
        assert_eq!(
            after.metadata.last_known_good_offset_hz,
            Some(after.metadata.frequency),
            "the offset the caller demonstrably heard us on must be recorded \
             as known-good so a later stall Reverts here instead of Switching blind"
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 5): after the Hound's
    /// procedure-mandated QSY into the response region, the POST-QSY offset —
    /// not the pre-QSY low calling offset — must be the recorded known-good.
    ///
    /// The QSY is driven by the very same `RespondingToCq -> SendingReport`
    /// advance that records known-good, so without the re-anchor a later
    /// `SendingReport` stall would read "we are off our known-good offset"
    /// and emit `Revert` straight back to the low calling offset, undoing the
    /// QSY and putting our R+report where the Fox is no longer listening.
    #[tokio::test]
    async fn hound_qsy_reanchors_the_known_good_offset_so_a_stall_never_reverts_it() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();

        // A Hound QSO calls the Fox low (calling region) with the Fox's own
        // frequency latched as `partner_freq`.
        let fox_rx_hz = 300.0;
        let qso_id = manager
            .engage_hound(DX, fox_rx_hz, None, None)
            .await
            .unwrap();
        let before = manager.get_qso(qso_id).await.unwrap();
        let calling_offset = before.metadata.frequency;
        assert!(
            calling_offset <= config_call_max(),
            "precondition: the Hound calls inside the low calling region, got {calling_offset}"
        );

        // The Fox answers with a signal report: RespondingToCq ->
        // SendingReport, which triggers the one-shot QSY.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -12,
                },
                format!("{OUR} {DX} -12"),
                fox_rx_hz,
                Some(-12.0),
            )
            .await
            .unwrap();

        let after = manager.get_qso(qso_id).await.unwrap();
        assert!(
            after.metadata.hound_qsyed,
            "precondition: the Fox's report must trigger the Hound QSY"
        );
        let qsyed = after.metadata.frequency;
        assert!(
            (qsyed - calling_offset).abs() > f64::EPSILON,
            "precondition: the QSY must actually move us out of the calling region"
        );
        assert_eq!(
            after.metadata.last_known_good_offset_hz,
            Some(qsyed),
            "the known-good anchor must follow the QSY into the response region"
        );

        // Now the Fox goes silent. Two stalled `SendingReport` rearm cycles
        // must produce a Switch (we're still on our known-good offset), NOT a
        // Revert to the abandoned low calling offset.
        let entered_at = after.metadata.last_call_at.unwrap();
        let _ = drain(&mut rx);
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(15))
            .await;
        manager
            .rearm_manual_calls_at(entered_at + Duration::seconds(30))
            .await;
        let action = drain(&mut rx).into_iter().find_map(|e| match e {
            QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
            _ => None,
        });
        assert_eq!(
            action,
            Some(OffsetAction::Switch { avoid_hz: qsyed }),
            "a stalled Hound must search within the response region, never \
             Revert to the pre-QSY calling offset"
        );
    }

    /// The default Hound calling-region ceiling, for the precondition above.
    fn config_call_max() -> f64 {
        HoundRegions::default().call_max_hz
    }

    /// PAN-72 (Codex round 4 on PR #350, finding 2): the Hound-region
    /// constraint must be revalidated INSIDE the commit, not merely used to
    /// steer the coordinator's earlier resolution.
    ///
    /// The drain reads the QSO (and therefore its Hound phase) with `get_qso`,
    /// resolves against that snapshot, and only then awaits
    /// `apply_tx_offset_switch`. For an operator-forced `u` nudge there is
    /// deliberately no `raised_at_generation`, so the advance guard cannot
    /// catch a QSY that lands in that window either. This test drives exactly
    /// that sequence: resolve a calling-region offset, let the Fox's report
    /// perform the QSY, then commit the now-stale offset.
    #[tokio::test]
    async fn a_hound_qsy_across_the_commit_window_refuses_the_stale_calling_offset() {
        let manager = manager_auto(test_config());
        let fox_rx_hz = 300.0;
        let qso_id = manager
            .engage_hound(DX, fox_rx_hz, None, None)
            .await
            .unwrap();

        // What the coordinator's pre-resolution `get_qso` would have seen: a
        // pre-QSY Hound, pinned to the low calling region. Pick a different
        // in-region offset — a legitimate calling-region relocation.
        let before = manager.get_qso(qso_id).await.unwrap();
        assert!(!before.metadata.hound_qsyed, "precondition: pre-QSY");
        let regions = HoundRegions::default();
        let resolved_hz = (before.metadata.frequency + 100.0).min(regions.call_max_hz);
        assert!(
            (resolved_hz - before.metadata.frequency).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ
                && resolved_hz >= regions.call_min_hz
                && resolved_hz <= regions.call_max_hz,
            "precondition: the resolved offset is a real move inside the calling region"
        );

        // ...and now the Fox answers, before the awaited commit runs: the
        // mandatory calling-region -> response-region QSY fires.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -12,
                },
                format!("{OUR} {DX} -12"),
                fox_rx_hz,
                Some(-12.0),
            )
            .await
            .unwrap();
        let qsyed_hz = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
        assert!(
            qsyed_hz >= regions.response_min_hz,
            "precondition: the QSY moved us into the response region, got {qsyed_hz}"
        );

        // Operator-forced (no generation token), exactly as the `u` nudge is.
        let err = manager
            .apply_tx_offset_switch(qso_id, resolved_hz, None)
            .await
            .expect_err("a calling-region offset must not commit onto a QSY'd Hound");
        assert!(
            matches!(
                err,
                QsoManagerError::OffsetActionOutsideHoundRegion { qso_id: id, .. } if id == qso_id
            ),
            "expected OffsetActionOutsideHoundRegion, got {err:?}"
        );
        assert!(
            err.is_expected_offset_action_refusal(),
            "a QSY racing the commit is an ordinary race, not a fault"
        );
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.frequency,
            qsyed_hz,
            "the refused action must leave the procedure-mandated QSY intact"
        );
    }

    /// The control: an in-region relocation on a QSY'd Hound still commits.
    #[tokio::test]
    async fn a_hound_switch_inside_the_live_response_region_still_commits() {
        let manager = manager_auto(test_config());
        let fox_rx_hz = 300.0;
        let qso_id = manager
            .engage_hound(DX, fox_rx_hz, None, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -12,
                },
                format!("{OUR} {DX} -12"),
                fox_rx_hz,
                Some(-12.0),
            )
            .await
            .unwrap();
        let regions = HoundRegions::default();
        let qsyed_hz = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
        let target = (qsyed_hz + 200.0).min(regions.response_max_hz);
        assert!(
            (target - qsyed_hz).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ,
            "precondition: the target is a real move inside the response region"
        );

        assert_eq!(
            manager
                .apply_tx_offset_switch(qso_id, target, None)
                .await
                .expect("an in-region relocation must still commit"),
            target
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 3): a queued action drained
    /// after the QSO completed must mutate nothing.
    ///
    /// Completed entries stay in the map (and stay in the coordinator's
    /// active snapshots for a 45-second trailing-73 grace window), so the
    /// lookup still succeeds. `QsoState::set_frequency` refuses terminal
    /// states, but `metadata.frequency` had no such guard — the write went
    /// through, the state kept the real logged offset, and the drain then
    /// mirrored the phantom offset into `active_tx_offsets` while the queued
    /// final frame still went out on the original one.
    #[tokio::test]
    async fn apply_tx_offset_switch_refuses_a_terminal_qso_without_mutating_it() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
        // The DX closes early with 73: SendingReport -> Completed.
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{OUR} {DX} 73"),
                FREQ,
                Some(-7.0),
            )
            .await
            .unwrap();

        let before = manager.get_qso(qso_id).await.unwrap();
        assert!(
            before.state.is_terminal(),
            "precondition: the QSO must be terminal, got {:?}",
            before.state
        );

        let err = manager
            .apply_tx_offset_switch(qso_id, FREQ + 400.0, None)
            .await
            .expect_err("a terminal QSO must refuse an offset action");
        assert!(
            matches!(err, QsoManagerError::QsoNotActive { qso_id: id } if id == qso_id),
            "expected QsoNotActive, got {err:?}"
        );
        assert!(
            err.is_expected_offset_action_refusal(),
            "a post-completion action is an expected race, not a fault"
        );

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.frequency, before.metadata.frequency,
            "metadata.frequency must not be rewritten on a terminal QSO — the \
             coordinator mirrors this value into active_tx_offsets"
        );
        assert_eq!(
            after.state.frequency(),
            before.state.frequency(),
            "the logged offset is the historical record and must not move"
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 8): the coordinator drains
    /// its request mailbox only once per 15-second slot, so the DX can send
    /// an advancing frame between an action being raised and committed. The
    /// advance already reset `stall_cycles` and marked the CURRENT offset
    /// known-good, so applying the stale request would move the QSO off the
    /// offset that just demonstrably worked.
    #[tokio::test]
    async fn apply_tx_offset_switch_discards_a_request_the_qso_has_advanced_past() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let raised_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .advance_generation;
        assert_eq!(raised_at, 0, "a fresh QSO starts at generation 0");

        // The DX comes back before the once-per-slot drain runs.
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
        let advanced = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            advanced.metadata.advance_generation,
            raised_at + 1,
            "a forward advance must bump the staleness token"
        );
        assert_eq!(
            advanced.metadata.last_known_good_offset_hz,
            Some(FREQ),
            "precondition: the current offset is now the known-good one"
        );

        let err = manager
            .apply_tx_offset_switch(qso_id, FREQ + 400.0, Some(raised_at))
            .await
            .expect_err("a superseded request must be discarded");
        assert!(
            matches!(
                err,
                QsoManagerError::OffsetActionStale { qso_id: id, raised_at: r, current: c }
                    if id == qso_id && r == raised_at && c == raised_at + 1
            ),
            "expected OffsetActionStale, got {err:?}"
        );
        assert!(err.is_expected_offset_action_refusal());
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.frequency,
            FREQ,
            "the QSO must stay on the offset the DX just answered on"
        );

        // A request raised at the CURRENT generation still commits normally,
        // and an operator-forced nudge (`None`) is never staleness-checked.
        let applied = manager
            .apply_tx_offset_switch(qso_id, FREQ + 400.0, Some(raised_at + 1))
            .await
            .expect("a current-generation request must still commit");
        assert_eq!(applied, FREQ + 400.0);
        let forced = manager
            .apply_tx_offset_switch(qso_id, FREQ + 500.0, None)
            .await
            .expect("an operator-forced nudge is never stale");
        assert_eq!(forced, FREQ + 500.0);
    }

    /// PAN-79 / PAN-72 (Codex round 3 on PR #350, finding 2): a resolution that
    /// lands back on the QSO's CURRENT offset relocated nothing, so it must not
    /// be committed as a successful move.
    ///
    /// The allocator returns `avoid_hz` unchanged when every candidate is
    /// excluded — its documented "no valid relocation" signal — and the drain
    /// hands that straight to this method. Treating it as a success cleared
    /// `stall_cycles`, destroying valid accumulated stall evidence (an operator
    /// `u` nudge on a QSO with a partial streak is the live case), delaying the
    /// automatic recovery by another full `qso_stall_switch_after` window, and
    /// emitting a `TxOffsetApplied` that claims a move that never happened.
    #[tokio::test]
    async fn apply_tx_offset_switch_refuses_a_no_op_and_keeps_the_stall_evidence() {
        let manager = manager_auto(test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // A partial (below-threshold) stall streak, exactly what an operator
        // `u` nudge lands on top of.
        {
            let mut qsos = manager.qsos.write().await;
            qsos.get_mut(&qso_id).unwrap().metadata.stall_cycles = 3;
        }
        let _ = drain(&mut rx);

        let err = manager
            .apply_tx_offset_switch(qso_id, FREQ, None)
            .await
            .expect_err("a resolution equal to the current offset relocates nothing");
        assert!(
            matches!(
                err,
                QsoManagerError::OffsetActionNoOp { qso_id: id, offset_hz }
                    if id == qso_id && offset_hz == FREQ
            ),
            "expected OffsetActionNoOp, got {err:?}"
        );
        assert!(
            err.is_expected_offset_action_refusal(),
            "an exhausted candidate space is an expected outcome, not a fault — \
             the drain must log+skip it, not warn"
        );
        assert!(
            !matches!(
                err,
                QsoManagerError::QsoNotActive { .. } | QsoManagerError::OffsetActionStale { .. }
            ),
            "a no-op must stay distinguishable from the staleness refusals"
        );

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.stall_cycles, 3,
            "the accumulated stall evidence must survive a no-op relocation, or \
             automatic recovery is delayed by another full threshold"
        );
        assert_eq!(after.metadata.frequency, FREQ);
        assert_eq!(
            after.metadata.partner_freq, None,
            "a no-op must not manufacture a Tx/Rx split"
        );
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetApplied { .. })),
            "a no-op must not announce a move that never happened, got {events:?}"
        );

        // A float-noise-sized "move" is the same no-op (the 1.0 Hz tolerance
        // this method already uses for its partner_freq latch)...
        let err = manager
            .apply_tx_offset_switch(qso_id, FREQ + 0.5, None)
            .await
            .expect_err("a sub-Hz move is still a no-op");
        assert!(matches!(err, QsoManagerError::OffsetActionNoOp { .. }));

        // ...while a real relocation still commits, and clears the streak.
        let applied = manager
            .apply_tx_offset_switch(qso_id, FREQ + 400.0, None)
            .await
            .expect("a real move must still commit");
        assert_eq!(applied, FREQ + 400.0);
        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after.metadata.stall_cycles, 0);
    }

    /// In the default Hold mode, `stall_cycles` may still tick (cheap
    /// tracking) but `TxOffsetActionNeeded` must never be emitted, no matter
    /// how many silent rearm cycles elapse.
    #[tokio::test]
    async fn hold_mode_never_emits_tx_offset_action_needed() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = QsoManager::new(config); // default Hold
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // Well past the threshold (2) worth of silent rearm cycles.
        for i in 1..=6i64 {
            manager
                .rearm_manual_calls_at(opened_at + Duration::seconds(15 * i))
                .await;
        }

        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
            "Hold mode must never emit TxOffsetActionNeeded, got {events:?}"
        );
    }

    // ── partner_freq bookkeeping across an offset switch ──────────────────
    //
    // `QsoState::frequency()` is dual-purposed: it is our TX offset AND —
    // whenever `metadata.partner_freq` is `None` — the RX-side baseline every
    // routing/relevance and drift gate keys on
    // (`metadata.partner_freq.unwrap_or(qso_freq)`). A stall switch moves
    // only OUR side, so once it writes the new offset into the state it must
    // also latch `partner_freq` to the offset it moved AWAY from — exactly
    // the bookkeeping `compute_manual_tx_offset` already performs for the
    // analogous "our TX diverges from the DX's frequency" case.

    /// The DX's real replies keep arriving at the PRE-switch offset. They must
    /// still route to this QSO after the switch; re-baselining the relevance
    /// gate onto our new TX offset silently orphans every one of them (the
    /// 400 Hz move here is far outside `ESTABLISHED_FREQ_TOLERANCE_HZ`).
    #[tokio::test]
    async fn stall_switch_keeps_routing_dx_replies_at_the_old_offset() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let before = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            before.metadata.partner_freq, None,
            "precondition: the ordinary Tx=Rx path opens with partner_freq = None"
        );

        let new_offset = FREQ + 400.0;
        let applied = manager
            .apply_tx_offset_switch(qso_id, new_offset, None)
            .await
            .unwrap();
        assert_eq!(applied, new_offset);

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.frequency, new_offset,
            "our TX offset must move to the switched-to value"
        );
        assert_eq!(after.state.frequency(), Some(new_offset));
        assert_eq!(
            after.metadata.partner_freq,
            Some(FREQ),
            "the switch must latch where we still expect to HEAR the DX (the \
             offset we moved away from), or every RX-side gate re-baselines \
             onto our own new TX offset"
        );

        let report = MessageType::SignalReport {
            to_station: OUR.into(),
            from_station: DX.into(),
            report: -11,
        };
        assert!(
            manager.is_message_relevant(&after.state, &after.metadata, &report, FREQ, false),
            "the DX's reply at the pre-switch offset must stay relevant"
        );
        assert_eq!(
            manager.find_qsos_for_message(&report, FREQ).await,
            vec![qso_id],
            "the DX's reply at the pre-switch offset must still route to this QSO"
        );
    }

    /// The drift half of the same regression: with the RX baseline wrongly
    /// pinned to our new TX offset, the DX's own (entirely unchanged)
    /// frequency reads as a drift candidate, and two sightings >=5s apart
    /// relatch `metadata.frequency` and the state frequency straight back onto
    /// the offset the switch existed to escape.
    #[tokio::test]
    async fn stall_switch_does_not_relatch_back_onto_the_abandoned_offset() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let new_offset = FREQ + 400.0;
        manager
            .apply_tx_offset_switch(qso_id, new_offset, None)
            .await
            .unwrap();

        let report = MessageType::SignalReport {
            to_station: OUR.into(),
            from_station: DX.into(),
            report: -11,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, FREQ, t0)
            .await;
        assert_eq!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .pending_freq_drift,
            None,
            "a DX sitting exactly where we left it is not a drift candidate"
        );
        manager
            .maybe_confirm_frequency_drift_at(&report, FREQ, t0 + Duration::seconds(6))
            .await;

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.frequency, new_offset,
            "the switch must survive: no relatch back onto the abandoned offset"
        );
        assert_eq!(after.state.frequency(), Some(new_offset));
        assert_eq!(after.metadata.partner_freq, Some(FREQ));
    }

    /// A QSO that already diverged (Hound, an offset hold, a collision nudge,
    /// a passband clamp) already carries the correct "where we hear the DX".
    /// A stall switch is purely a TX-side move, so it must not clobber it.
    #[tokio::test]
    async fn stall_switch_preserves_an_existing_partner_freq() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_with(
                DX.into(),
                FREQ,
                None,
                CallInitiation::Manual,
                Some(2400.0),
                false,
            )
            .await
            .unwrap();

        manager
            .apply_tx_offset_switch(qso_id, FREQ + 400.0, None)
            .await
            .unwrap();

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.partner_freq,
            Some(2400.0),
            "an already-latched partner_freq is where we hear the DX; a TX-side \
             switch must leave it untouched"
        );
        assert_eq!(after.metadata.frequency, FREQ + 400.0);
    }

    /// A no-op switch (a Revert onto the offset we are already sitting on)
    /// must not manufacture a `partner_freq` — the QSO is still plainly Tx=Rx,
    /// and inventing a split would change nothing but add a field the drift
    /// gate then treats as a deliberate divergence.
    ///
    /// Since PAN-79 / round 3 finding 2 such a switch is refused outright
    /// (`OffsetActionNoOp`) rather than committed as a mutation-free success,
    /// which subsumes this guarantee — asserted here as well so the
    /// partner_freq contract stays pinned at its own level.
    #[tokio::test]
    async fn no_op_switch_leaves_partner_freq_none() {
        let manager = manager_auto(test_config());
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();

        let err = manager
            .apply_tx_offset_switch(qso_id, FREQ, None)
            .await
            .expect_err("a no-op switch is refused, not committed");
        assert!(matches!(err, QsoManagerError::OffsetActionNoOp { .. }));

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.partner_freq, None,
            "a switch that changes nothing must leave the Tx=Rx path untouched"
        );
        assert_eq!(after.metadata.frequency, FREQ);
    }

    /// A manual CQ QSO that nobody has answered yet has NO DX to keep pointing
    /// at: `their_callsign()` is `None`, and the offset we move away from is
    /// our own abandoned CQ frequency, not a partner's. Latching it into
    /// `partner_freq` re-points every RX-side gate at an offset no one is
    /// transmitting on — and because a pre-establishment QSO is judged with
    /// the TIGHT `FREQ_TOLERANCE_HZ` (15 Hz), not the 100 Hz established
    /// bound, a caller answering us at the offset we are ACTUALLY calling on
    /// is rejected forever and a duplicate QSO object gets spawned in its
    /// place. The latch must therefore be gated on an established DX identity,
    /// the same discriminator `is_message_relevant`/`classify_relevance`
    /// already use to choose between the two tolerances.
    #[tokio::test]
    async fn cq_stall_switch_leaves_partner_freq_none_and_keeps_answering_callers_routing() {
        const CALLER: &str = "W9XYZ";

        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();
        let qso_id = manager.start_cq_manual(FREQ, None, false).await.unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // Two silent rearm cycles: nobody has come back to our CQ, so the
        // stall detector trips and asks for a Switch off this offset.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(30))
            .await;
        let action = drain(&mut rx).into_iter().find_map(|e| match e {
            QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
            _ => None,
        });
        assert_eq!(
            action,
            Some(OffsetAction::Switch { avoid_hz: FREQ }),
            "a silent CQ must stall into a Switch off its current offset"
        );

        // The coordinator commits the resolved Switch: we are now CALLING CQ
        // on the new offset.
        let new_offset = FREQ + 400.0;
        let applied = manager
            .apply_tx_offset_switch(qso_id, new_offset, None)
            .await
            .unwrap();
        assert_eq!(applied, new_offset);

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(after.metadata.frequency, new_offset);
        assert_eq!(after.state.frequency(), Some(new_offset));
        assert!(
            after.state.their_callsign().is_none(),
            "precondition: an unanswered CQ has no DX identity yet"
        );
        assert_eq!(
            after.metadata.partner_freq, None,
            "there is no DX at the abandoned CQ offset to point the RX gates \
             at — latching one strands the QSO at a frequency nobody uses"
        );

        // A caller answers us where we are actually calling: the new offset.
        let answer = MessageType::CqResponse {
            calling_station: OUR.into(),
            responding_station: CALLER.into(),
            grid: Some("FN31".into()),
        };
        assert!(
            manager.is_message_relevant(&after.state, &after.metadata, &answer, new_offset, false),
            "an answer at the offset we are calling on must stay relevant"
        );
        assert_eq!(
            manager.find_qsos_for_message(&answer, new_offset).await,
            vec![qso_id],
            "an answer at the offset we are calling on must route to this CQ, \
             not spawn a duplicate QSO"
        );
    }

    /// PAN-72 (Codex round 4 on PR #350, finding 3): the resend that CROSSES
    /// the stall threshold goes out on the PRE-switch offset — the same
    /// `rearm_manual_calls_at` pass emits it and raises the action — and the
    /// coordinator's once-per-slot drain then moves the QSO. A caller who
    /// answers that transmission is answering the offset we have just left.
    ///
    /// For an unanswered manual CQ that is fatal: `CallingCq` now carries the
    /// NEW offset, round 1's fix deliberately leaves `partner_freq` `None`
    /// (there is no DX at the abandoned offset to point the RX gates at), and
    /// the pre-establishment gate is only 15 Hz wide — so the answer is
    /// rejected outright and `maybe_answer_caller` spawns a duplicate QSO in
    /// its place.
    ///
    /// This drives the real sequence rather than the pieces: rearm to the
    /// threshold, commit the switch the threshold raised, then deliver the
    /// caller's answer at the offset the triggering resend actually used.
    #[tokio::test]
    async fn a_caller_answering_the_pre_switch_resend_still_routes_to_the_cq() {
        const CALLER: &str = "W9XYZ";

        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();
        let qso_id = manager.start_cq_manual(FREQ, None, false).await.unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        let _ = drain(&mut rx);

        // Two silent rearm cycles. The SECOND one both re-emits our CQ at FREQ
        // (the frame a caller can answer) and trips the stall threshold.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(30))
            .await;
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                QsoEvent::MessageToSend { frequency, .. } if (*frequency - FREQ).abs() < f64::EPSILON
            )),
            "precondition: the threshold-crossing rearm re-sends our CQ on the \
             PRE-switch offset, {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                QsoEvent::TxOffsetActionNeeded {
                    action: OffsetAction::Switch { avoid_hz },
                    ..
                } if (*avoid_hz - FREQ).abs() < f64::EPSILON
            )),
            "precondition: the same pass raises the Switch, {events:?}"
        );

        // The coordinator's drain commits the switch a moment later.
        let new_offset = FREQ + 400.0;
        assert_eq!(
            manager
                .apply_tx_offset_switch(qso_id, new_offset, None)
                .await
                .unwrap(),
            new_offset
        );

        // ...and only now does the caller who heard our PRE-switch CQ answer,
        // on the offset that CQ actually went out on.
        let answer = MessageType::CqResponse {
            calling_station: OUR.into(),
            responding_station: CALLER.into(),
            grid: Some("FN31".into()),
        };
        assert_eq!(
            manager.find_qsos_for_message(&answer, FREQ).await,
            vec![qso_id],
            "an answer to the frame we transmitted on the pre-switch offset \
             must still route to this CQ, not be gated away into a duplicate QSO"
        );

        // And it must actually advance the QSO, not merely route.
        manager
            .process_message(answer, format!("{OUR} {CALLER} FN31"), FREQ, Some(-12.0))
            .await
            .unwrap();
        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.state.their_callsign(),
            Some(CALLER),
            "the pre-switch answer must establish the QSO, got {:?}",
            after.state
        );
    }

    /// The pre-switch RX baseline is a bounded grace, not a permanent second
    /// window: once `PRE_SWITCH_OFFSET_GRACE` lapses, an answer at the
    /// abandoned offset is judged against the current offset again — otherwise
    /// the QSO would keep accepting traffic at a frequency it no longer uses.
    #[tokio::test]
    async fn the_pre_switch_rx_baseline_lapses_with_its_grace() {
        const CALLER: &str = "W9XYZ";

        let manager = manager_auto(test_config());
        let qso_id = manager.start_cq_manual(FREQ, None, false).await.unwrap();
        let new_offset = FREQ + 400.0;
        manager
            .apply_tx_offset_switch(qso_id, new_offset, None)
            .await
            .unwrap();

        let mut progress = manager.get_qso(qso_id).await.unwrap();
        let answer = MessageType::CqResponse {
            calling_station: OUR.into(),
            responding_station: CALLER.into(),
            grid: Some("FN31".into()),
        };
        assert!(
            manager.is_message_relevant(&progress.state, &progress.metadata, &answer, FREQ, false),
            "precondition: inside the grace the vacated offset is still accepted"
        );

        progress.metadata.pre_switch_offset = Some((
            FREQ,
            Utc::now() - PRE_SWITCH_OFFSET_GRACE - Duration::seconds(1),
        ));
        assert!(
            !manager.is_message_relevant(&progress.state, &progress.metadata, &answer, FREQ, false),
            "a lapsed pre-switch baseline must stop accepting traffic at the \
             abandoned offset"
        );
        assert!(
            manager.is_message_relevant(
                &progress.state,
                &progress.metadata,
                &answer,
                new_offset,
                false
            ),
            "the current offset must keep matching regardless"
        );
    }

    /// The other half of round 4, finding 3: an ESTABLISHED QSO's reply to the
    /// pre-switch frame does route (the switch latches `partner_freq` to the
    /// vacated offset), but the advance it drives must NOT record the brand-new
    /// offset — which has never been transmitted on — as `last_known_good`.
    /// Doing so poisons the Revert target: a later stall would "revert" to the
    /// offset it is already on, and the genuinely-proven one is lost.
    #[tokio::test]
    async fn an_advance_answering_the_pre_switch_frame_credits_the_old_offset() {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened_at = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();

        // Stall out on FREQ; the threshold-crossing rearm re-sends there.
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(15))
            .await;
        manager
            .rearm_manual_calls_at(opened_at + Duration::seconds(30))
            .await;
        let new_offset = FREQ + 400.0;
        manager
            .apply_tx_offset_switch(qso_id, new_offset, None)
            .await
            .unwrap();

        // The DX answers the pre-switch frame — still transmitting where it
        // always was, which the switch latched as `partner_freq`.
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

        let after = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(
            after.metadata.frequency, new_offset,
            "precondition: the QSO is on the post-switch offset"
        );
        assert_eq!(
            after.metadata.last_known_good_offset_hz,
            Some(FREQ),
            "the DX answered the frame we sent on {FREQ} Hz — that, not the \
             never-transmitted {new_offset} Hz, is the proven offset"
        );
    }
}

#[cfg(test)]
mod has_active_or_recent_qso_tests {
    //! Tests for `has_active_or_recent_qso_with` — the completion-aware dedup
    //! gate used by `maybe_answer_caller` to suppress post-completion duplicate
    //! QSOs (fix for the ZL1UHD-style four-73 bug).
    use super::*;
    use chrono::Duration;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn manager() -> QsoManager {
        QsoManager::new(QsoManagerConfig {
            our_callsign: "W1ABC".into(),
            our_grid: Some("FN42".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        })
    }

    /// Minimal `QsoMetadata` for a QSO with `their_callsign`.
    fn meta(their_callsign: &str) -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id: Uuid::new_v4(),
            our_callsign: "W1ABC".into(),
            their_callsign: Some(their_callsign.into()),
            frequency: 1500.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".into(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: CallInitiation::Auto,
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
        }
    }

    /// Insert a `QsoProgress` with the given state directly into the manager's
    /// map (mirrors the pattern used by other test modules).
    async fn insert(manager: &QsoManager, state: QsoState, their_call: &str) -> Uuid {
        let id = Uuid::new_v4();
        let progress = QsoProgress {
            state,
            state_history: vec![],
            messages: vec![],
            metadata: meta(their_call),
        };
        manager.qsos.write().await.insert(id, progress);
        id
    }

    /// PAN-25 round 2 (Codex P2): `has_active_or_recent_qso_with` — the
    /// `maybe_answer_caller` preflight's own gate — was still callsign-only,
    /// unaffected by PAN-25's earlier band-scoping fixes to the OTHER two
    /// suppression checks. A station worked on 20m must not suppress a
    /// direct caller reply on 40m here either.
    #[tokio::test]
    async fn has_active_or_recent_qso_with_is_band_scoped() {
        let m = manager();
        let mut progress = QsoProgress {
            state: QsoState::Completed {
                their_callsign: "ZL1UHD".into(),
                their_report: -12,
                our_report: -7,
                frequency: 1500.0,
                grid_square: None,
                completed_at: Utc::now() - Duration::seconds(5),
                duration_seconds: 60,
            },
            state_history: vec![],
            messages: vec![],
            metadata: meta("ZL1UHD"),
        };
        // Explicit 20m completion band, bypassing real dial machinery for a
        // direct, focused test of the band comparison itself.
        progress.metadata.completed_rf_frequency_hz = Some(14_074_000.0);
        let id = Uuid::new_v4();
        m.qsos.write().await.insert(id, progress);

        assert!(
            m.has_active_or_recent_qso_with(
                "ZL1UHD",
                14_074_000.0,
                std::time::Duration::from_secs(120)
            )
            .await,
            "a same-band (20m) direct call must still be suppressed"
        );
        assert!(
            !m.has_active_or_recent_qso_with(
                "ZL1UHD",
                7_074_000.0,
                std::time::Duration::from_secs(120)
            )
            .await,
            "a different-band (40m) direct call must NOT be suppressed by a 20m completion"
        );
    }

    // ── within-window: recently completed → true ─────────────────────────────

    #[tokio::test]
    async fn recently_completed_within_window_returns_true() {
        let m = manager();
        // Completed 5 seconds ago — well within the 120 s window.
        let completed_state = QsoState::Completed {
            their_callsign: "ZL1UHD".into(),
            their_report: -12,
            our_report: -7,
            frequency: 1500.0,
            grid_square: None,
            completed_at: Utc::now() - Duration::seconds(5),
            duration_seconds: 60,
        };
        insert(&m, completed_state, "ZL1UHD").await;

        assert!(
            m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "recently-completed QSO must block duplicate creation"
        );
    }

    // ── outside window: stale completed → false ───────────────────────────────

    #[tokio::test]
    async fn completed_outside_window_returns_false() {
        let m = manager();
        // Completed 200 seconds ago — outside the 120 s window.
        let completed_state = QsoState::Completed {
            their_callsign: "ZL1UHD".into(),
            their_report: -12,
            our_report: -7,
            frequency: 1500.0,
            grid_square: None,
            completed_at: Utc::now() - Duration::seconds(200),
            duration_seconds: 60,
        };
        insert(&m, completed_state, "ZL1UHD").await;

        assert!(
            !m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "stale (>window) completed QSO must NOT block a new one"
        );
    }

    // ── active QSO → true ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn active_qso_returns_true() {
        let m = manager();
        let active_state = QsoState::RespondingToCq {
            target_callsign: "ZL1UHD".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        insert(&m, active_state, "ZL1UHD").await;

        assert!(
            m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "active QSO must block duplicate"
        );
    }

    // ── different callsign → false ────────────────────────────────────────────

    #[tokio::test]
    async fn different_callsign_returns_false() {
        let m = manager();
        let completed_state = QsoState::Completed {
            their_callsign: "ZL1UHD".into(),
            their_report: -12,
            our_report: -7,
            frequency: 1500.0,
            grid_square: None,
            completed_at: Utc::now() - Duration::seconds(5),
            duration_seconds: 60,
        };
        insert(&m, completed_state, "ZL1UHD").await;

        assert!(
            !m.has_active_or_recent_qso_with("K9ZZ", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "QSO with a different callsign must not match"
        );
    }

    // ── no QSOs at all → false ────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_manager_returns_false() {
        let m = manager();
        assert!(
            !m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "no QSOs → always false"
        );
    }

    // ── compound-call equivalence ─────────────────────────────────────────────

    #[tokio::test]
    async fn compound_call_matches_base() {
        let m = manager();
        // QSO was logged under the compound form EA8/G8BCG.
        let completed_state = QsoState::Completed {
            their_callsign: "EA8/G8BCG".into(),
            their_report: -12,
            our_report: -7,
            frequency: 1500.0,
            grid_square: None,
            completed_at: Utc::now() - Duration::seconds(5),
            duration_seconds: 60,
        };
        insert(&m, completed_state, "EA8/G8BCG").await;

        // Query with the bare base call — must still match.
        assert!(
            m.has_active_or_recent_qso_with("G8BCG", 1500.0, std::time::Duration::from_secs(120))
                .await,
            "compound EA8/G8BCG QSO must match bare base G8BCG query"
        );
    }
}

#[cfg(test)]
mod hound_tests {
    use super::*;
    use crate::states::{GridSquares, SignalReports};
    use chrono::Utc;
    use pancetta_core::slot::SlotParity;
    use std::collections::HashMap;
    use uuid::Uuid;

    // ── QsoMetadata default field values ────────────────────────────────────

    fn make_metadata() -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id: Uuid::new_v4(),
            our_callsign: "K5ARH".into(),
            their_callsign: Some("KH8/K5ARH".into()),
            frequency: 600.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".into(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity: Some(SlotParity::Even),
            initiated_by: CallInitiation::Manual,
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
        }
    }

    #[test]
    fn qso_metadata_hound_defaults_false() {
        let md = make_metadata();
        assert!(!md.hound, "hound must default to false");
    }

    #[test]
    fn qso_metadata_partner_freq_defaults_none() {
        let md = make_metadata();
        assert_eq!(md.partner_freq, None, "partner_freq must default to None");
    }

    #[test]
    fn qso_metadata_hound_qsyed_defaults_false() {
        let md = make_metadata();
        assert!(!md.hound_qsyed, "hound_qsyed must default to false");
    }

    // ── hound_offset_for ────────────────────────────────────────────────────

    #[test]
    fn hound_offset_in_range() {
        for seed in &["K5ARH", "VK9X", "KH8B", "FT5ZM", "3B9FR"] {
            let off = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
            assert!(
                (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(&off),
                "offset {off} out of range [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}] for seed {seed}"
            );
        }
    }

    #[test]
    fn hound_offset_deterministic() {
        let seed = "K5ARH";
        let a = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        let b = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        assert_eq!(a, b, "same seed must produce same offset (determinism)");
    }

    #[test]
    fn hound_offset_spreads_distinct_seeds() {
        // These two seeds must produce different offsets under FNV-1a.
        let a = hound_offset_for("K5ARH", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        let b = hound_offset_for("VK9X", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        assert_ne!(a, b, "distinct seeds must produce distinct offsets");
    }

    #[test]
    fn hound_offset_snapped_to_5hz() {
        for seed in &["K5ARH", "VK9X", "KH8B", "FT5ZM", "ZL9A"] {
            let off = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
            let remainder = off % 5.0;
            assert!(
                remainder.abs() < 1e-9,
                "offset {off} for seed {seed} is not a multiple of 5 Hz (remainder {remainder})"
            );
        }
    }

    #[test]
    fn hound_offset_degenerate_lo_eq_hi() {
        let result = hound_offset_for("K5ARH", 500.0, 500.0);
        assert_eq!(result, 500.0, "lo == hi must return lo");
    }

    // ── engage_hound constructor ─────────────────────────────────────────────

    fn hound_test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: "K5ARH".to_string(),
            our_grid: Some("EM20".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: "FT8".to_string(),
        }
    }

    /// engage_hound creates a QSO in RespondingToCq with correct Hound metadata.
    ///
    /// Verifies:
    /// - state == RespondingToCq
    /// - metadata.hound == true
    /// - metadata.partner_freq == Some(fox_freq)
    /// - metadata.frequency in [300.0, 900.0]  (the low calling region)
    /// - metadata.initiated_by == Manual
    /// - metadata.role == Caller
    /// - tx_parity == Some(SlotParity::Odd)  (opposite of Even DX parity)
    #[tokio::test]
    async fn engage_hound_creates_qso_with_correct_metadata() {
        let manager = QsoManager::new(hound_test_config());

        let qso_id = manager
            .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
            .await
            .expect("engage_hound should succeed");

        let progress = manager.get_qso(qso_id).await.unwrap();

        // State
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "expected RespondingToCq, got {:?}",
            progress.state
        );

        // Hound-specific fields
        assert!(
            progress.metadata.hound,
            "metadata.hound must be true for a Hound QSO"
        );
        assert_eq!(
            progress.metadata.partner_freq,
            Some(1800.0),
            "metadata.partner_freq must be Some(fox_freq)"
        );

        // Low calling offset in [300, 900]
        let freq = progress.metadata.frequency;
        assert!(
            (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(&freq),
            "metadata.frequency {freq} must be in [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}]"
        );

        // initiation / role
        assert_eq!(
            progress.metadata.initiated_by,
            CallInitiation::Manual,
            "Hound v1 QSO must be Manual"
        );
        assert_eq!(
            progress.metadata.role,
            QsoRole::Caller,
            "Hound is the Caller (we chase the Fox)"
        );

        // tx_parity = opposite of dx_parity (Even → Odd)
        assert_eq!(
            progress.metadata.tx_parity,
            Some(SlotParity::Odd),
            "tx_parity must be opposite of Fox's Even parity"
        );
    }

    /// engage_hound emits an opening MessageToSend on the LOW calling offset
    /// whose text targets the Fox callsign and includes our station callsign.
    #[tokio::test]
    async fn engage_hound_emits_opening_message_on_low_offset() {
        let manager = QsoManager::new(hound_test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
            .await
            .expect("engage_hound should succeed");

        // Collect all events emitted during the call.
        let mut msg_events: Vec<(f64, MessageType)> = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend {
                qso_id: eid,
                message,
                frequency,
                ..
            } = ev
            {
                if eid == qso_id {
                    msg_events.push((frequency, message));
                }
            }
        }

        assert!(
            !msg_events.is_empty(),
            "at least one MessageToSend must be emitted for the opening call"
        );

        // The first MessageToSend is our CqResponse call to the Fox.
        let (event_freq, message) = &msg_events[0];

        // Frequency must be in the low calling region.
        assert!(
            (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(event_freq),
            "opening MessageToSend frequency {event_freq} must be in the low calling region [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}]"
        );

        // Message must be CqResponse directed at the Fox with our callsign.
        match message {
            MessageType::CqResponse {
                calling_station,
                responding_station,
                ..
            } => {
                assert_eq!(
                    calling_station, "D2UY",
                    "CqResponse calling_station must be the Fox"
                );
                assert_eq!(
                    responding_station, "K5ARH",
                    "CqResponse responding_station must be our callsign"
                );
            }
            other => panic!("expected CqResponse, got {:?}", other),
        }
    }

    /// engage_hound is deterministic: two calls for the same Fox callsign yield
    /// the same low calling offset.
    #[tokio::test]
    async fn engage_hound_same_fox_same_low_offset() {
        let manager = QsoManager::new(hound_test_config());

        let a = manager
            .engage_hound("D2UY", 1800.0, None, Some(SlotParity::Even))
            .await
            .expect("first engage_hound");
        // The second call supersedes the first (same callsign, same band) —
        // that's fine, we just need both to agree on the offset.
        let b = manager
            .engage_hound("D2UY", 1800.0, None, Some(SlotParity::Even))
            .await
            .expect("second engage_hound");

        let freq_a = manager.get_qso(a).await.map(|p| p.metadata.frequency);
        let freq_b = manager.get_qso(b).await.unwrap().metadata.frequency;

        // If a was superseded its progress may be gone (Error); b always exists.
        // Either way both were computed from the same seed so they're identical.
        let expected_offset = hound_offset_for("D2UY", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        assert_eq!(
            freq_b, expected_offset,
            "second QSO offset must equal hound_offset_for(D2UY)"
        );
        if let Ok(fa) = freq_a {
            assert_eq!(
                fa, expected_offset,
                "first QSO offset must equal hound_offset_for(D2UY)"
            );
        }
    }

    /// engage_hound with dx_parity=None (Fox's parity unknown) now LATCHES a
    /// concrete provisional parity via `engage_hound` → `respond_to_cq_with`'s
    /// `None`-dx_parity fallback (Batch 2 remediation), instead of leaving
    /// `tx_parity` permanently `None` (the old behavior, which made the TX
    /// scheduler re-resolve "nearest next slot" independently every
    /// subsequent slot and alternate parity — the exact bug this fixes).
    #[tokio::test]
    async fn engage_hound_no_parity_when_dx_parity_unknown() {
        let manager = QsoManager::new(hound_test_config());

        let qso_id = manager
            .engage_hound("VK9M", 1500.0, None, None)
            .await
            .expect("engage_hound with no parity");

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.tx_parity.is_some(),
            "tx_parity should be latched to a concrete parity even when \
             dx_parity is None (provisional latch, not left None forever)"
        );
        assert!(
            progress.metadata.tx_parity_provisional,
            "a parity latched with no observed dx_parity must be marked provisional"
        );
        assert!(
            progress.metadata.hound,
            "hound flag must still be set even with no parity"
        );
    }

    // ── Hound QSY-on-report (Task 5) ────────────────────────────────────────

    /// Helper: drain all pending events from the broadcast receiver.
    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Collect all `MessageToSend` events from a drained list.
    fn message_to_sends(events: &[QsoEvent]) -> Vec<(f64, MessageType)> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend {
                    message, frequency, ..
                } => Some((*frequency, message.clone())),
                _ => None,
            })
            .collect()
    }

    /// T5-1: Full Hound exchange end-to-end.
    ///
    /// `engage_hound("D2UY", 1800.0, Some("JI64"), Some(Even))`
    /// → assert opening call is low (300–900 Hz).
    /// Feed Fox `SignalReport` directed at us, arriving at Fox's freq 1800 Hz
    /// → assert:
    ///   - state == `SendingReport`,
    ///   - `metadata.hound_qsyed == true`,
    ///   - `metadata.frequency ∈ [1000, 2700]` (QSY'd up),
    ///   - `metadata.partner_freq` still `Some(1800.0)` (unchanged),
    ///   - emitted `ReportAck` (`<D2UY> <us> R-NN`) rides the QSY'd offset.
    /// Then feed Fox `FinalConfirmation` at 1800 Hz
    /// → assert the QSO reaches `Completed`.
    #[tokio::test]
    async fn hound_qsy_on_fox_report_full_exchange() {
        let manager = QsoManager::new(hound_test_config());
        let mut rx = manager.subscribe();

        // Open Hound QSO targeting D2UY who is on 1800 Hz.
        let qso_id = manager
            .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
            .await
            .expect("engage_hound must succeed");

        // Check the opening call is in the LOW region.
        let opening_events = drain_events(&mut rx);
        let opening_sends = message_to_sends(&opening_events);
        assert!(
            !opening_sends.is_empty(),
            "opening MessageToSend must be emitted"
        );
        let (open_freq, _) = &opening_sends[0];
        assert!(
            (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(open_freq),
            "opening call freq {open_freq} must be in [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}]"
        );

        // ── Fox answers with a signal report directed at us, at Fox's freq ──
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -12,
                },
                "K5ARH D2UY -12".into(),
                1800.0, // Fox's decoded frequency (partner_freq)
                Some(-12.0),
            )
            .await
            .expect("process_message for Fox SignalReport must succeed");

        // ── Assert QSY ──
        let after_report_events = drain_events(&mut rx);
        let progress = manager.get_qso(qso_id).await.unwrap();

        // State must have advanced to SendingReport.
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "state must be SendingReport after Fox report, got {:?}",
            progress.state
        );

        // hound_qsyed must be set (fire-once gate).
        assert!(
            progress.metadata.hound_qsyed,
            "hound_qsyed must be true after the first Fox report"
        );

        // TX offset must have QSY'd into the response region.
        let qsy_freq = progress.metadata.frequency;
        assert!(
            (HOUND_RESPONSE_MIN_HZ..=HOUND_RESPONSE_MAX_HZ).contains(&qsy_freq),
            "metadata.frequency {qsy_freq} must be in response region [{HOUND_RESPONSE_MIN_HZ}, {HOUND_RESPONSE_MAX_HZ}] after QSY"
        );

        // partner_freq must be unchanged (Fox's RX offset is fixed).
        assert_eq!(
            progress.metadata.partner_freq,
            Some(1800.0),
            "partner_freq must remain Some(1800.0) after QSY"
        );

        // The emitted ReportAck must ride the QSY'd offset (not the old low one).
        let sends = message_to_sends(&after_report_events);
        let report_acks: Vec<_> = sends
            .iter()
            .filter(|(_, m)| matches!(m, MessageType::ReportAck { .. }))
            .collect();
        assert!(
            !report_acks.is_empty(),
            "a ReportAck must be emitted after the Fox's SignalReport"
        );
        let (ack_freq, ack_msg) = &report_acks[0];
        assert!(
            (HOUND_RESPONSE_MIN_HZ..=HOUND_RESPONSE_MAX_HZ).contains(ack_freq),
            "ReportAck must be emitted on the QSY'd offset {ack_freq} in [{HOUND_RESPONSE_MIN_HZ}, {HOUND_RESPONSE_MAX_HZ}]"
        );
        // ReportAck must be <D2UY> <us> R-NN.
        match ack_msg {
            MessageType::ReportAck {
                to_station,
                from_station,
                ..
            } => {
                assert_eq!(to_station, "D2UY", "ReportAck must be addressed to Fox");
                assert_eq!(from_station, "K5ARH", "ReportAck must be from us");
            }
            other => panic!("expected ReportAck, got {:?}", other),
        }

        // ── Fox sends RR73 (at Fox's freq) → QSO completes ──
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    from_station: "D2UY".into(),
                    to_station: "K5ARH".into(),
                },
                "K5ARH D2UY RR73".into(),
                1800.0, // still at Fox's decoded frequency
                Some(-10.0),
            )
            .await
            .expect("process_message for Fox RR73 must succeed");

        let final_progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(final_progress.state, QsoState::Completed { .. }),
            "QSO must reach Completed after Fox RR73, got {:?}",
            final_progress.state
        );
    }

    /// T5-2: QSY fires even in `TxFreqMode::Hold`.
    ///
    /// The Hound QSY is procedure-mandated — independent of the autonomous
    /// silence-driven stall detector's gate (`TxFreqMode::Auto`). In the
    /// default Hold mode the operator's TX offset is "sticky" for the stall
    /// detector, but the Hound QSY MUST still move to the response region on
    /// the Fox's first report.
    #[tokio::test]
    async fn hound_qsy_fires_in_hold_mode() {
        // Default manager uses Hold (the tx_freq_mode default is Hold).
        let manager = QsoManager::new(hound_test_config());
        // Confirm it is in Hold mode (the stall detector would NOT fire in this mode).
        // We do NOT set_tx_freq_mode_source to Auto — leave it as Hold.

        let qso_id = manager
            .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
            .await
            .expect("engage_hound must succeed");

        let before_freq = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
        assert!(
            (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(&before_freq),
            "before QSY, offset {before_freq} must be in the low calling region"
        );

        // Fox answers with a report — QSY must fire regardless of Hold mode.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -12,
                },
                "K5ARH D2UY -12".into(),
                1800.0,
                Some(-12.0),
            )
            .await
            .expect("process_message for Fox SignalReport in Hold mode must succeed");

        let progress = manager.get_qso(qso_id).await.unwrap();
        let qsy_freq = progress.metadata.frequency;
        assert!(
            (HOUND_RESPONSE_MIN_HZ..=HOUND_RESPONSE_MAX_HZ).contains(&qsy_freq),
            "metadata.frequency {qsy_freq} must QSY into [{HOUND_RESPONSE_MIN_HZ}, {HOUND_RESPONSE_MAX_HZ}] even in Hold mode"
        );
        assert!(
            progress.metadata.hound_qsyed,
            "hound_qsyed must be true after QSY in Hold mode"
        );
    }

    /// Task 6 (updated): engage_hound must NOT insert a bare "HOUND" key into
    /// metadata.tags — that would produce an invalid `<HOUND:4>true` ADIF field
    /// (non-`APP_`-prefixed) that can trip LoTW.  The COMMENT "HOUND" signal and
    /// the `APP_PANCETTA_HOUND` machine-readable field are both written by
    /// `AdifProcessor::qso_to_adif` from `metadata.hound` directly, so the tag
    /// is redundant AND harmful.  What we do require is `metadata.hound == true`.
    #[tokio::test]
    async fn engage_hound_no_stray_hound_tag_in_metadata() {
        let manager = QsoManager::new(hound_test_config());

        let qso_id = manager
            .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
            .await
            .expect("engage_hound must succeed");

        let progress = manager.get_qso(qso_id).await.unwrap();

        // The hound flag must be set — it drives COMMENT+APP_PANCETTA_HOUND in ADIF.
        assert!(
            progress.metadata.hound,
            "metadata.hound must be true after engage_hound"
        );

        // The bare "HOUND" tag must NOT be present — it would generate an invalid
        // ADIF field `<HOUND:4>true` that can fail LoTW validation.
        assert!(
            !progress.metadata.tags.contains_key("HOUND"),
            "metadata.tags must NOT contain a bare 'HOUND' key (would produce invalid ADIF); got tags={:?}",
            progress.metadata.tags
        );
    }

    /// T5-3: Non-Hound Caller QSO is completely unaffected by the QSY hook.
    ///
    /// A normal `respond_to_cq_manual` QSO (hound=false) receiving a
    /// `SignalReport` from the DX must NOT move its TX offset — `metadata.frequency`
    /// stays at the latched value, and `hound_qsyed` stays false.
    #[tokio::test]
    async fn non_hound_caller_qso_not_affected_by_qsy_hook() {
        let manager = QsoManager::new(hound_test_config());
        // Normal (non-Hound) QSO at 1500 Hz.
        let normal_freq = 1500.0;
        let qso_id = manager
            .respond_to_cq_manual("D2UY".into(), normal_freq, None)
            .await
            .expect("respond_to_cq_manual must succeed");

        let before = manager.get_qso(qso_id).await.unwrap();
        assert!(
            !before.metadata.hound,
            "hound must be false for a non-Hound QSO"
        );
        assert_eq!(
            before.metadata.frequency, normal_freq,
            "frequency must be the latched value before any report"
        );

        // DX sends a signal report.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -9,
                },
                "K5ARH D2UY -09".into(),
                normal_freq,
                Some(-9.0),
            )
            .await
            .expect("process_message for SignalReport must succeed");

        let after = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(after.state, QsoState::SendingReport { .. }),
            "state must advance to SendingReport"
        );
        // Frequency must NOT move.
        assert_eq!(
            after.metadata.frequency, normal_freq,
            "non-Hound QSO: metadata.frequency must NOT change on report (got {})",
            after.metadata.frequency
        );
        // hound_qsyed must stay false.
        assert!(
            !after.metadata.hound_qsyed,
            "non-Hound QSO: hound_qsyed must stay false"
        );
    }
}

#[cfg(test)]
mod deconflict_tests {
    use super::deconflict_offset;

    const LO: f64 = 300.0;
    const HI: f64 = 2700.0;
    const SEP: f64 = 75.0;

    #[test]
    fn deconflict_clear_candidate_returned_unchanged() {
        // 1500 Hz is well away from 800 Hz — must come back untouched.
        let result = deconflict_offset(1500.0, &[800.0], SEP, LO, HI);
        assert_eq!(result, 1500.0, "clear candidate must be returned unchanged");
    }

    #[test]
    fn deconflict_collision_nudges_at_least_min_sep() {
        // Candidate sits exactly on an occupied offset — must move by ≥ min_sep.
        let result = deconflict_offset(1500.0, &[1500.0], SEP, LO, HI);
        assert!(
            (LO..=HI).contains(&result),
            "result {result} must be within [{LO}, {HI}]"
        );
        assert!(
            (result - 1500.0).abs() >= SEP,
            "result {result} must be ≥ {SEP} Hz from the occupied offset 1500.0"
        );
    }

    #[test]
    fn deconflict_near_collision_within_min_sep() {
        // Candidate 1520 Hz is only 20 Hz from occupied 1500 Hz (< 75 Hz sep) — must move.
        let result = deconflict_offset(1520.0, &[1500.0], SEP, LO, HI);
        assert!(
            (LO..=HI).contains(&result),
            "result {result} must be within [{LO}, {HI}]"
        );
        assert!(
            (result - 1500.0).abs() >= SEP,
            "result {result} must be ≥ {SEP} Hz from 1500.0 (was only 20 Hz away)"
        );
    }

    #[test]
    fn deconflict_bracketed_gap_finds_clear_slot() {
        // Candidate 1500 Hz is bracketed by 1400 and 1600 Hz; the 100 Hz gap
        // between them is narrower than 2 * min_sep so neither side clears
        // within the bracket — the search must escape to a clear slot elsewhere.
        let result = deconflict_offset(1500.0, &[1400.0, 1600.0], SEP, LO, HI);
        assert!(
            (LO..=HI).contains(&result),
            "result {result} must be within [{LO}, {HI}]"
        );
        assert!(
            (result - 1400.0).abs() >= SEP,
            "result {result} must be ≥ {SEP} Hz from 1400.0"
        );
        assert!(
            (result - 1600.0).abs() >= SEP,
            "result {result} must be ≥ {SEP} Hz from 1600.0"
        );
    }

    #[test]
    fn deconflict_empty_occupied_clamps_candidate() {
        // No occupied offsets; candidate 2900 is above HI — must clamp to HI.
        let result = deconflict_offset(2900.0, &[], SEP, LO, HI);
        assert_eq!(
            result, HI,
            "out-of-range candidate with no occupied must clamp to hi"
        );
    }

    #[test]
    fn deconflict_deterministic() {
        // Same inputs twice must produce identical outputs.
        let occupied = [800.0_f64, 1200.0, 1800.0];
        let a = deconflict_offset(1500.0, &occupied, SEP, LO, HI);
        let b = deconflict_offset(1500.0, &occupied, SEP, LO, HI);
        assert_eq!(a, b, "deconflict_offset must be deterministic");
    }
}

#[cfg(test)]
mod timeout_config_tests {
    use super::TimeoutConfig;

    #[test]
    fn timeout_config_default_qso_stall_switch_after_is_4() {
        let config = TimeoutConfig::default();
        assert_eq!(config.qso_stall_switch_after, 4);
    }
}
