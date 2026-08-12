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

/// Consecutive identical-DX-frame count at which an in-progress QSO performs a
/// one-time TX-frequency hop. The DX repeating the same message this many times
/// without advancing is the operator's cue that it cannot copy our replies —
/// most plausibly because something is colliding with us on our held TX offset.
/// (FT8 receivers decode the entire passband, so our offset is otherwise
/// irrelevant to whether the DX hears us; we therefore hold it for the QSO
/// unless this stuck condition trips.)
pub const DX_STUCK_REPEAT_THRESHOLD: u32 = 4;

/// Amount (Hz) by which a stuck QSO hops its TX audio offset, wrapping within
/// [`TX_OFFSET_MIN_HZ`, `TX_OFFSET_MAX_HZ`]. Sized to clear a co-channel FT8
/// signal (50 Hz wide) and its neighbours by a comfortable margin.
const STUCK_TX_HOP_HZ: f64 = 300.0;

/// Usable FT8 audio passband for our TX offset (Hz). Matches the collision
/// detector's clamp range in `autonomous.rs`.
pub const TX_OFFSET_MIN_HZ: f64 = 300.0;
/// Upper bound of the usable FT8 audio passband for our TX offset (Hz).
pub const TX_OFFSET_MAX_HZ: f64 = 2700.0;

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

/// Hop a stuck QSO's TX audio offset by [`STUCK_TX_HOP_HZ`], wrapping back to
/// the low end of the passband when it would exceed [`TX_OFFSET_MAX_HZ`]. Pure
/// and deterministic so the move is unit-testable; the goal is simply to vacate
/// the current offset, not to find a spectrally-optimal one (the engine has no
/// spectral snapshot — picking a clear frequency is the allocator's job at QSO
/// open). Inputs are the audio offset (pre dial-frequency stamping).
fn stuck_hopped_offset(current: f64) -> f64 {
    let next = current + STUCK_TX_HOP_HZ;
    if next > TX_OFFSET_MAX_HZ {
        // Wrap into the low half of the band, preserving the sub-hop remainder.
        TX_OFFSET_MIN_HZ + (next - TX_OFFSET_MAX_HZ)
    } else {
        next
    }
    .clamp(TX_OFFSET_MIN_HZ, TX_OFFSET_MAX_HZ)
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
}

/// Default for [`TimeoutConfig::repetitive_tx_timeout_secs`] (5 minutes).
fn default_repetitive_tx_timeout_secs() -> u64 {
    300
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
    /// from the coordinator. The stuck-DX TX-offset hop only fires in `Auto`
    /// mode; in the default `Hold` mode the operator's picked offset is sticky
    /// and never moved autonomously. Defaults to a private `Hold` atomic so
    /// unit tests and any caller that never injects a source keep the
    /// hold-the-frequency behavior.
    tx_freq_mode: Arc<std::sync::atomic::AtomicU8>,
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
    /// one (within `within`). Used to suppress the always-answer-callers path
    /// from opening a second QSO for a station we just finished working — the
    /// bounded auto-resend-73 path already handles a DX that didn't copy our
    /// 73. Explicit operator re-work bypasses this gate entirely (it uses the
    /// `StartQso` → `respond_to_cq_manual` path, not `maybe_answer_caller`).
    ///
    /// Compound-call-aware: `EA8/G8BCG` and `G8BCG` are the same station.
    pub async fn has_active_or_recent_qso_with(
        &self,
        callsign: &str,
        within: std::time::Duration,
    ) -> bool {
        let qsos = self.qsos.read().await;
        let now = chrono::Utc::now();
        let window_secs = within.as_secs() as i64;
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
            // Recently completed? (`completed_at` lives in QsoState::Completed { .. }.)
            if let QsoState::Completed { completed_at, .. } = &p.state {
                return now.signed_duration_since(*completed_at).num_seconds() <= window_secs;
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
            // Default Hold: the stuck-DX hop stays off unless the coordinator
            // injects a shared mode atomic and the operator switches to Auto.
            tx_freq_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Hold.as_u8(),
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

    /// Share the coordinator's TX-frequency-mode atomic so the stuck-DX hop
    /// respects the operator's Hold/Auto choice at runtime. Pass the same
    /// `Arc<AtomicU8>` the TUI toggle updates (encoded via
    /// [`pancetta_core::TxFreqMode::as_u8`]). If never called, the manager keeps
    /// its private `Hold` default (no autonomous frequency changes).
    pub fn set_tx_freq_mode_source(&mut self, source: Arc<std::sync::atomic::AtomicU8>) {
        self.tx_freq_mode = source;
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

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
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
            last_rx_text: None,
            dx_repeat_count: 0,
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
            last_rx_text: None,
            dx_repeat_count: 0,
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
        {
            let mut qsos = self.qsos.write().await;
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.metadata.hound = true;
                // NOTE: do NOT insert "HOUND" into metadata.tags — that would
                // produce a bare `<HOUND:4>true` ADIF field, which is not a
                // valid ADIF name (must be `APP_`-prefixed per the ADIF spec)
                // and can trip LoTW. The human-readable COMMENT "HOUND" and the
                // machine-readable `APP_PANCETTA_HOUND` field are both written
                // by `AdifProcessor::qso_to_adif` from `metadata.hound` directly.
            }
        }

        info!(
            "Hound: engaging Fox {} on partner_freq={:.1} Hz, calling low @ {:.1} Hz: {}",
            fox_call, fox_freq, low_offset, qso_id
        );

        Ok(qso_id)
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
            last_rx_text: None,
            dx_repeat_count: 0,
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
            last_rx_text: None,
            dx_repeat_count: 0,
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
                metadata.frequency += dial as f64;
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
        // Must match is_message_relevant's ESTABLISHED_FREQ_TOLERANCE_HZ (100.0) and
        // the concept of FREQ_TOLERANCE_HZ (15.0) for "counts as the same spot" —
        // redeclared here as local constants rather than shared, since this fix
        // deliberately does not touch is_message_relevant at all.
        const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;
        const DRIFT_CONFIRM_TOLERANCE_HZ: f64 = 15.0;
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
            )
            .await?;

        // Did this received frame advance us up the responder ladder? Computed
        // here (before `old_state`/`new_state` are moved into emit_state_change)
        // for the stuck-DX detector below. Off-ladder advances (CQer flow) read
        // as `false`, which is harmless: the detector's identical-text guard
        // means a genuinely-progressing QSO (whose DX frames change each step)
        // never accumulates repeats regardless.
        let dx_frame_advanced = Self::ladder_rank(&new_state) > Self::ladder_rank(&old_state);

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
                let mut m = progress.metadata.clone();
                // `m.frequency` is the audio offset within the slot. The logged
                // RF frequency is the rig dial plus that offset (WSJT-X logs the
                // actual on-air frequency, not the dial). Without this the ADIF
                // recorded BAND 0MHZ / FREQ ~0.001 from the bare offset.
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                if dial > 0 {
                    m.frequency += dial as f64;
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

        // Stuck-DX TX-frequency hold/escape (operator request): we hold our TX
        // offset for the whole QSO so long as it is "working". The cue that it
        // has stopped working is the DX repeating the *same* frame without the
        // QSO advancing — they aren't copying our replies, most plausibly a
        // collision on our held offset. After `DX_STUCK_REPEAT_THRESHOLD`
        // identical non-advancing frames we hop our TX offset once and reset the
        // counter. A forward advance resets the counter to 0 (the hold is fine);
        // a *different* non-advancing frame resets it to 1. Applies to both
        // Manual and Auto QSOs.
        // The stuck-DX hop is an autonomous TX-offset change, so it only fires
        // in Auto mode. In the default Hold mode the operator's picked offset is
        // sticky — we still TRACK the repeat streak (cheap, and ready if they
        // switch to Auto) but never move the frequency.
        let tx_auto = pancetta_core::TxFreqMode::from_u8(
            self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_auto_change();
        let rx_text = message.raw_text.trim().to_uppercase();
        if let Some(progress) = qsos.get_mut(&qso_id) {
            if dx_frame_advanced {
                progress.metadata.dx_repeat_count = 0;
                progress.metadata.last_rx_text = Some(rx_text);
            } else if !rx_text.is_empty()
                && progress.metadata.last_rx_text.as_deref() == Some(rx_text.as_str())
            {
                progress.metadata.dx_repeat_count =
                    progress.metadata.dx_repeat_count.saturating_add(1);
            } else if !rx_text.is_empty() {
                progress.metadata.dx_repeat_count = 1;
                progress.metadata.last_rx_text = Some(rx_text);
            }

            if tx_auto && progress.metadata.dx_repeat_count >= DX_STUCK_REPEAT_THRESHOLD {
                let old_off = progress.metadata.frequency;
                let new_off = stuck_hopped_offset(old_off);
                progress.metadata.frequency = new_off;
                progress.metadata.pending_freq_drift = None;
                progress.metadata.dx_repeat_count = 0;
                // Keep the reply we are about to emit this cycle on the new
                // offset (the captured `qso_frequency` was the pre-hop value).
                qso_frequency = new_off;
                warn!(
                    target: "tx.freq",
                    qso_id = %qso_id,
                    dx = progress.metadata.their_callsign.as_deref().unwrap_or("?"),
                    "DX stuck (repeated frame x{}) — hopping our TX offset {:.0} Hz -> {:.0} Hz to clear a possible collision",
                    DX_STUCK_REPEAT_THRESHOLD, old_off, new_off
                );
            }

            // Hound QSY: when the Fox answers with a signal report
            // (RespondingToCq → SendingReport), the Hound must move its TX
            // offset up into the response region (1000–2700 Hz) and send the
            // R+report (`ReportAck`) there — the defining Hound procedure move.
            //
            // Fires exactly once per QSO (`hound_qsyed` gate). Executes
            // INDEPENDENT of `TxFreqMode` (procedure-mandated, not an
            // autonomous optimisation — unlike the Auto-gated stuck-hop above).
            //
            // Pattern mirrors the stuck-hop: mutate BOTH `metadata.frequency`
            // (used as `qso_frequency` on the NEXT process_message call) AND
            // `qso_frequency` (rides the ReportAck emitted this cycle). We also
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

    async fn determine_state_transition(
        &self,
        qso_id: QsoId,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
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
            for (&qso_id, progress) in qsos.iter() {
                if !progress.state.is_active() {
                    continue;
                }

                let verdict = self.classify_relevance(
                    &progress.state,
                    &progress.metadata,
                    message_type,
                    frequency,
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
    ) -> Relevance {
        let relevant = self.is_message_relevant(state, metadata, message_type, frequency);
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
        let within_frequency_gate = state.frequency().is_none_or(|qso_frequency| {
            let match_frequency = metadata.partner_freq.unwrap_or(qso_frequency);
            let tolerance = if state.their_callsign().is_some() {
                100.0
            } else {
                15.0
            };
            (match_frequency - frequency).abs() <= tolerance
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
    ) -> bool {
        // Frequency tolerance tightened from 50 Hz → 15 Hz to reduce
        // cross-QSO message bleed-through in multi-QSO mode. FT8 frame-to-
        // frame drift is typically < 6 Hz on a stable transceiver, so 15 Hz
        // covers normal operation while shrinking the window an attacker
        // can exploit. (Security review 2026-04-29 C-1.)
        const FREQ_TOLERANCE_HZ: f64 = 15.0;
        // B15 fix: once a QSO is ESTABLISHED (we know the contra callsign and
        // are past CallingCq/Idle), allow a wider drift so an actively-
        // answering DX that has drifted beyond the tight window is NOT dropped.
        // The match arms below already require from == DX && to == us && the
        // state-appropriate message, which unambiguously identifies our partner
        // — at that point callsign+state continuity wins over the freq window
        // (catalog B15). We WIDEN the gate (to 100 Hz) rather than re-latch the
        // QSO's stored frequency here: `is_message_relevant` takes `&self` and
        // holds only a read lock, so it cannot mutate state; 100 Hz comfortably
        // covers realistic transceiver drift / micro-QSY within a contact while
        // still bounding how far a stray station can be from our partner's
        // latched offset. The tight 15 Hz gate is kept for INITIAL / ambiguous
        // matching (CallingCq, Idle, and any non-matching message) so two
        // different stations are never merged into one QSO.
        const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;

        let matched = match (state, message_type) {
            // We're calling CQ. The responder's callsign is whoever is in the
            // `responding_station` field; the message must be addressed to us.
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    calling_station, ..
                },
            ) => self.is_us(calling_station),

            // A4 (routing half): a caller answered our CQ with a bare signal
            // report (grid skipped) — "<us> <them> -NN". Route it to this
            // CallingCq QSO so the transition arm can step CQ → report. Only
            // addressed-to-us reports qualify (any from_station, since we don't
            // yet know who will answer).
            (QsoState::CallingCq { .. }, MessageType::SignalReport { to_station, .. }) => {
                self.is_us(to_station)
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
        // Hound mode: when `metadata.partner_freq` is `Some`, the Fox transmits
        // on a *different* frequency than we do (Hound calls low; Fox replies
        // somewhere in [1000, 4000] Hz). We must match the incoming Fox frame
        // against the Fox's RX offset (`partner_freq`), NOT our TX offset
        // (`state.frequency()`). When `partner_freq` is `None` (every normal
        // QSO) `unwrap_or(qso_freq)` falls back to the latched `qso_freq`,
        // producing byte-identical behavior to before this change.
        if let Some(qso_freq) = state.frequency() {
            let match_freq = metadata.partner_freq.unwrap_or(qso_freq);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            if (match_freq - frequency).abs() > tolerance {
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
    async fn find_recently_completed_manual_qso_for_at(
        &self,
        callsign: &str,
        frequency: f64,
        within: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| match &p.state {
                    QsoState::Completed { completed_at, .. }
                        if p.metadata.initiated_by == CallInitiation::Manual
                            && crate::utils::frequency_to_band(p.metadata.frequency)
                                == want_band
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
            let completed_metadata = if is_completed {
                progress.metadata.reports = SignalReports {
                    sent: Some(our_report),
                    received: Some(their_report_val),
                };
                progress.metadata.end_time = Some(now);
                let mut m = progress.metadata.clone();
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                if dial > 0 {
                    m.frequency += dial as f64;
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
            // Repetitive-TX watchdog (operator request): if a QSO has sat in the
            // same active TX state — i.e. we've been re-sending the SAME message
            // without the DX advancing us — longer than repetitive_tx_timeout_secs,
            // retire it. Applies to BOTH manual and auto QSOs and is checked first
            // so it bounds "stuck sending the same thing" even while the manual
            // keep-call watchdog (below) would otherwise keep re-arming. A forward
            // state advance resets the state's `started_at`, so a healthy,
            // progressing QSO never trips this.
            let active_tx_state = matches!(
                progress.state,
                QsoState::CallingCq { .. }
                    | QsoState::RespondingToCq { .. }
                    | QsoState::SendingReport { .. }
                    | QsoState::WaitingForReport { .. }
                    | QsoState::WaitingForConfirmation { .. }
                    | QsoState::SendingConfirmation { .. }
            );
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
                // QsoFailureReason::Timeout for every push site above.
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
                last_rx_text: None,
                dx_repeat_count: 0,
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
            .determine_state_transition(qso_id, &state, &spoof, None, CallInitiation::Auto)
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
            .determine_state_transition(Uuid::new_v4(), &state, &legit, None, CallInitiation::Auto)
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
        assert!(!manager.is_message_relevant(&state, &md, &spoof, 1500.0));
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
        let verdict = manager.classify_relevance(&state, &metadata, &impostor, 1500.0);
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
                manager.classify_relevance(&state, &metadata, &ordinary_traffic, frequency);
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
        assert!(manager.is_message_relevant(&state, &md, &legit, 1500.0));
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
        assert!(!manager.is_message_relevant(&state, &md, &legit, 1516.0));
        // 14 Hz off → accepted.
        assert!(manager.is_message_relevant(&state, &md, &legit, 1514.0));
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
        assert!(manager.is_message_relevant(&state, &md, &legit, 1516.0));
        assert!(manager.is_message_relevant(&state, &md, &legit, 1545.0));
        assert!(manager.is_message_relevant(&state, &md, &legit, 1600.0));
        // Beyond the 100 Hz established bound → rejected (still bounded).
        assert!(!manager.is_message_relevant(&state, &md, &legit, 1601.0));
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
            manager.is_message_relevant(&state, &md, &fox_report, 1800.0),
            "Hound: frame at Fox's offset must be relevant"
        );
        // Frame at 1810 Hz — still within 100 Hz ESTABLISHED bound of 1800.
        assert!(
            manager.is_message_relevant(&state, &md, &fox_report, 1810.0),
            "Hound: frame 10 Hz from Fox's offset must be relevant (within established tolerance)"
        );
        // Frame at OUR TX offset (600 Hz) but far from the Fox's RX offset —
        // must be rejected because 600 Hz is not close to partner_freq 1800 Hz.
        assert!(
            !manager.is_message_relevant(&state, &md, &fox_report, 600.0),
            "Hound: frame at our TX offset (far from Fox) must NOT be relevant"
        );
        // Frame beyond even the ESTABLISHED bound from partner_freq — rejected.
        assert!(
            !manager.is_message_relevant(&state, &md, &fox_report, 1901.0),
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
            manager.is_message_relevant(&state, &md, &legit, 1500.0),
            "regression: frame at state.frequency must be relevant when partner_freq=None"
        );
        // Frame far from QSO frequency → not relevant (unchanged).
        assert!(
            !manager.is_message_relevant(&state, &md, &legit, 2000.0),
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
        assert!(manager.is_message_relevant(&progress.state, &progress.metadata, &report, 856.0));
        assert!(!manager.is_message_relevant(&progress.state, &progress.metadata, &report, 2931.0));
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
        let qso_id = manager
            .engage_hound(
                "D2UY".into(),
                1800.0,
                Some("JI64".into()),
                Some(pancetta_core::slot::SlotParity::Even),
            )
            .await
            .unwrap();
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "D2UY".into(),
            report: -12,
        };
        let t0 = Utc::now();
        manager
            .maybe_confirm_frequency_drift_at(&report, 900.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&report, 900.0, t0 + chrono::Duration::seconds(6))
            .await;

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert_eq!(progress.metadata.pending_freq_drift, None);
        assert_eq!(progress.metadata.partner_freq, Some(1800.0));
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
            .determine_state_transition(Uuid::new_v4(), &state, &spoof, None, CallInitiation::Auto)
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
            .determine_state_transition(Uuid::new_v4(), &state, &spoof, None, CallInitiation::Auto)
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
            .determine_state_transition(Uuid::new_v4(), &state, &legit, None, CallInitiation::Auto)
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
mod stuck_dx_tests {
    //! Mid-QSO TX-frequency hold + stuck-DX escape (operator request).
    //!
    //! We hold the QSO's latched TX audio offset for the whole exchange so long
    //! as it is "working" (the DX keeps advancing). The escape: when the DX
    //! repeats the *same* frame `DX_STUCK_REPEAT_THRESHOLD` times without
    //! advancing — they can't copy our replies, most plausibly a collision on
    //! our offset — we hop the offset once and keep going.
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

    /// Manager in Auto TX-freq mode (the stuck-DX hop is active).
    fn manager_auto() -> QsoManager {
        let mut m = manager();
        m.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        m
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

    #[test]
    fn stuck_hop_adds_offset_then_wraps_within_band() {
        assert_eq!(super::stuck_hopped_offset(1500.0), 1800.0);
        // Near the top of the band it wraps into the low half.
        let hop = super::stuck_hopped_offset(2600.0);
        assert!(
            (super::TX_OFFSET_MIN_HZ..=super::TX_OFFSET_MAX_HZ).contains(&hop),
            "hop {hop} must stay in band"
        );
        assert!(hop < 2600.0, "a wrap must land below the starting offset");
        // Always inside the usable passband.
        for f in [300.0, 1000.0, 2699.0, 2700.0] {
            let h = super::stuck_hopped_offset(f);
            assert!((super::TX_OFFSET_MIN_HZ..=super::TX_OFFSET_MAX_HZ).contains(&h));
        }
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

    /// The DX repeating the SAME non-advancing frame trips the hop exactly at
    /// the threshold — not before.
    #[tokio::test]
    async fn identical_repeats_hop_tx_frequency_at_threshold() {
        let manager = manager_auto();
        let id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();

        // First report advances RespondingToCq → SendingReport (resets counter).
        let same = format!("{OUR} {DX} -07");
        send_report(&manager, -7, &same).await;
        assert_eq!(freq_of(&manager, id).await, FREQ);

        // Now the DX re-sends the identical report. Each repeat is non-advancing
        // (stays SendingReport). The hop fires on the DX_STUCK_REPEAT_THRESHOLD-th
        // identical repeat.
        for i in 1..DX_STUCK_REPEAT_THRESHOLD {
            send_report(&manager, -7, &same).await;
            assert_eq!(
                freq_of(&manager, id).await,
                FREQ,
                "must still hold before the threshold (repeat {i})"
            );
        }
        send_report(&manager, -7, &same).await;
        assert_eq!(
            freq_of(&manager, id).await,
            stuck_hopped_offset(FREQ),
            "the threshold-th identical repeat must hop the TX offset"
        );
    }

    /// In the default Hold mode the operator's offset is sticky: even a clearly
    /// stuck DX (identical frame well past the threshold) never moves it.
    #[tokio::test]
    async fn hold_mode_never_hops_even_when_stuck() {
        let manager = manager(); // default Hold
        let id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let same = format!("{OUR} {DX} -07");
        for _ in 0..(DX_STUCK_REPEAT_THRESHOLD * 2 + 1) {
            send_report(&manager, -7, &same).await;
            assert_eq!(
                freq_of(&manager, id).await,
                FREQ,
                "Hold mode must never move the TX offset"
            );
        }
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

    /// A *different* non-advancing frame resets the streak, so alternating
    /// frames never trip the hop.
    #[tokio::test]
    async fn changing_frames_never_hop() {
        let manager = manager();
        let id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

        // Alternate report values for well over the threshold — the text keeps
        // changing, so the streak never accumulates.
        for i in 0..(DX_STUCK_REPEAT_THRESHOLD * 3) {
            let r = -7 - (i as i8 % 2); // toggles -07 / -08
            send_report(&manager, r, &format!("{OUR} {DX} {r:03}")).await;
            assert_eq!(
                freq_of(&manager, id).await,
                FREQ,
                "changing frames must never hop (iter {i})"
            );
        }
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
            m.has_active_or_recent_qso_with("ZL1UHD", std::time::Duration::from_secs(120))
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
            !m.has_active_or_recent_qso_with("ZL1UHD", std::time::Duration::from_secs(120))
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
            m.has_active_or_recent_qso_with("ZL1UHD", std::time::Duration::from_secs(120))
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
            !m.has_active_or_recent_qso_with("K9ZZ", std::time::Duration::from_secs(120))
                .await,
            "QSO with a different callsign must not match"
        );
    }

    // ── no QSOs at all → false ────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_manager_returns_false() {
        let m = manager();
        assert!(
            !m.has_active_or_recent_qso_with("ZL1UHD", std::time::Duration::from_secs(120))
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
            m.has_active_or_recent_qso_with("G8BCG", std::time::Duration::from_secs(120))
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
    /// stuck-hop gate (`TxFreqMode::Auto`). In the default Hold mode the
    /// operator's TX offset is "sticky" for the stuck-hop, but the Hound QSY
    /// MUST still move to the response region on the Fox's first report.
    #[tokio::test]
    async fn hound_qsy_fires_in_hold_mode() {
        // Default manager uses Hold (the tx_freq_mode default is Hold).
        let manager = QsoManager::new(hound_test_config());
        // Confirm it is in Hold mode (the stuck-hop would NOT fire in this mode).
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
