//! Autonomous QSO Operator
//!
//! Sits above the existing `AutoSequencer` and `QsoManager`, making cycle-by-cycle
//! decisions: hunt for interesting CQs, call CQ when idle, manage even/odd slots,
//! and periodically listen on our TX slot to detect doubling.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::exchange::MessageExchange;
use crate::frequency::{
    DecodeHistory, DecodeRecord, FrequencyAllocatorConfig, FrequencyCandidate, PlacementSnapshot,
    SmartFrequencyAllocator, SpectralSnapshot, TimeSlot,
};
use crate::priority::{PriorityTier, TieredScore};
use crate::states::MessageType;
use crate::watchlist::DxWatchlist;
use pancetta_core::callsign::callsigns_match;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum AutonomousError {
    #[error("Autonomous operator not enabled")]
    NotEnabled,

    #[error("Invalid configuration: {message}")]
    Configuration { message: String },

    #[error("Slot timing error: {0}")]
    SlotTiming(String),
}

// ---------------------------------------------------------------------------
// Slot management
// ---------------------------------------------------------------------------

/// Even or odd 15-second FT8 time-slot parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotParity {
    Even,
    Odd,
}

impl SlotParity {
    /// Derive the current parity from a unix timestamp.
    pub fn from_unix_secs(secs: i64) -> Self {
        let slot_number = secs / 15;
        if slot_number % 2 == 0 {
            SlotParity::Even
        } else {
            SlotParity::Odd
        }
    }

    /// Return the current parity right now.
    pub fn current() -> Self {
        Self::from_unix_secs(Utc::now().timestamp())
    }
}

/// How the operator picks its TX parity.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotParityConfig {
    Even,
    Odd,
    /// Listen for a few slots and pick the quieter parity.
    #[default]
    Auto,
}

/// Whether to transmit, listen, or skip the current slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotDecision {
    /// This is our TX slot and policy says transmit.
    Transmit,
    /// This is our TX slot but policy says listen for collisions.
    Listen,
    /// Not our slot — do nothing.
    NotOurSlot,
}

/// Adaptive listen-cycle policy (good-neighbour collision detection).
#[derive(Debug, Clone)]
pub struct ListenPolicy {
    /// How often (in our-slot cycles) we listen instead of TX.
    pub listen_interval: u32,
    /// Counter of our-slot cycles since last listen.
    pub cycles_since_listen: u32,
    /// Consecutive listen cycles with no collision.
    pub clean_cycles: u32,
    /// Current collision state (elevated vigilance).
    pub collision_state: bool,
    /// Remaining elevated-vigilance cycles after a collision.
    pub collision_cooldown: u32,
}

impl ListenPolicy {
    pub fn new(config: &ListenCycleConfig) -> Self {
        Self {
            listen_interval: config.initial_interval,
            cycles_since_listen: 0,
            clean_cycles: 0,
            collision_state: false,
            collision_cooldown: 0,
        }
    }

    /// After a clean listen slot (no collision detected).
    pub fn record_clean_listen(&mut self, config: &ListenCycleConfig) {
        self.clean_cycles += 1;
        self.cycles_since_listen = 0;

        if self.collision_cooldown > 0 {
            self.collision_cooldown -= 1;
            if self.collision_cooldown == 0 {
                self.collision_state = false;
            }
        }

        // Back off to less-frequent listens after enough clean ones.
        if self.clean_cycles >= config.backoff_threshold && !self.collision_state {
            self.listen_interval = config.backoff_interval;
        }
    }

    /// After a collision is detected.
    pub fn record_collision(&mut self, config: &ListenCycleConfig) {
        self.collision_state = true;
        self.collision_cooldown = 10;
        self.clean_cycles = 0;
        self.listen_interval = config.collision_interval;
        self.cycles_since_listen = 0;
    }
}

/// Tracks the FT8 15-second time slots and our TX parity.
#[derive(Debug, Clone)]
pub struct SlotManager {
    pub our_slot: Option<SlotParity>,
    pub parity_config: SlotParityConfig,
    pub listen_policy: ListenPolicy,
    /// Counts used during auto-parity detection.
    auto_detect_slots_seen: u32,
    auto_detect_even_activity: u32,
    auto_detect_odd_activity: u32,
}

impl SlotManager {
    pub fn new(parity_config: SlotParityConfig, listen_config: &ListenCycleConfig) -> Self {
        let our_slot = match parity_config {
            SlotParityConfig::Even => Some(SlotParity::Even),
            SlotParityConfig::Odd => Some(SlotParity::Odd),
            SlotParityConfig::Auto => None,
        };

        Self {
            our_slot,
            parity_config,
            listen_policy: ListenPolicy::new(listen_config),
            auto_detect_slots_seen: 0,
            auto_detect_even_activity: 0,
            auto_detect_odd_activity: 0,
        }
    }

    /// Feed activity counts during auto-parity detection.
    pub fn record_slot_activity(&mut self, parity: SlotParity, decoded_count: u32) {
        if self.our_slot.is_some() {
            return; // Already decided.
        }

        self.auto_detect_slots_seen += 1;
        match parity {
            SlotParity::Even => self.auto_detect_even_activity += decoded_count,
            SlotParity::Odd => self.auto_detect_odd_activity += decoded_count,
        }

        // After 4 slots pick the quieter parity for TX.
        if self.auto_detect_slots_seen >= 4 {
            self.our_slot = Some(
                if self.auto_detect_even_activity <= self.auto_detect_odd_activity {
                    SlotParity::Even
                } else {
                    SlotParity::Odd
                },
            );
            info!(
                "Auto-detected TX parity: {:?} (even={}, odd={})",
                self.our_slot.expect("just assigned above"),
                self.auto_detect_even_activity,
                self.auto_detect_odd_activity,
            );
        }
    }

    /// Decide what to do in the current slot.
    pub fn should_transmit_this_slot(&mut self) -> SlotDecision {
        self.should_transmit_at(Utc::now().timestamp())
    }

    /// Decide what to do at a given unix timestamp (testable).
    pub fn should_transmit_at(&mut self, unix_secs: i64) -> SlotDecision {
        let current_parity = SlotParity::from_unix_secs(unix_secs);

        let Some(our_parity) = self.our_slot else {
            // Still auto-detecting — don't transmit.
            return SlotDecision::NotOurSlot;
        };

        if current_parity != our_parity {
            return SlotDecision::NotOurSlot;
        }

        // It's our slot. Check listen policy.
        self.listen_policy.cycles_since_listen += 1;
        if self.listen_policy.cycles_since_listen >= self.listen_policy.listen_interval {
            SlotDecision::Listen
        } else {
            SlotDecision::Transmit
        }
    }
}

// ---------------------------------------------------------------------------
// Collision detection
// ---------------------------------------------------------------------------

/// A decoded message with the fields the collision detector cares about.
#[derive(Debug, Clone)]
pub struct DecodedMessageInfo {
    pub callsign: Option<String>,
    pub frequency_hz: f64,
    pub snr: i32,
    pub message_text: String,
    /// The parity of the slot in which this message was decoded.
    /// `None` if the slot parity was not tracked at decode time.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
    /// hb-103 (Batch 32): decoder self-reported confidence in `[0, 1]`.
    /// `None` for messages from test scaffolding or pre-hb-103 code paths.
    /// Used together with `time_offset_s` and the filter trust set to
    /// compute a content score for autonomous TX gating.
    pub confidence: Option<f32>,
    /// hb-103 (Batch 32): time offset of the decode within its slot, in
    /// seconds. `None` for messages from test scaffolding or pre-hb-103
    /// code paths. Used as a content-score input.
    pub time_offset_s: Option<f64>,
    /// hb-247 (Batch 81): deterministic decode-origin ordinal from
    /// `ConfidenceFeatures::decode_origin` (0 = primary pass … 6 =
    /// sync relaxation, 7 = hb-252 BICM-ID rescue). Feeds the v3
    /// content score's lateness term (`origin / 6`, clamped to 1.0 —
    /// origin 7 saturates at the max penalty by design). `None` for
    /// pre-hb-247 paths and test scaffolding.
    pub decode_origin: Option<u8>,
}

/// Result of a collision check on a listen slot.
#[derive(Debug, Clone)]
pub struct CollisionResult {
    pub detected: bool,
    pub interfering_calls: Vec<String>,
}

/// Checks decoded messages from a listen slot for activity near our TX offset.
#[derive(Debug, Clone)]
pub struct CollisionDetector {
    pub our_tx_offset_hz: f64,
    pub tolerance_hz: f64,
}

impl CollisionDetector {
    pub fn new(our_tx_offset_hz: f64, tolerance_hz: f64) -> Self {
        Self {
            our_tx_offset_hz,
            tolerance_hz,
        }
    }

    pub fn check_for_collision(&self, decoded: &[DecodedMessageInfo]) -> CollisionResult {
        let mut interfering_calls = Vec::new();

        for msg in decoded {
            let delta = (msg.frequency_hz - self.our_tx_offset_hz).abs();
            if delta <= self.tolerance_hz {
                if let Some(ref call) = msg.callsign {
                    interfering_calls.push(call.clone());
                }
            }
        }

        CollisionResult {
            detected: !interfering_calls.is_empty(),
            interfering_calls,
        }
    }
}

// ---------------------------------------------------------------------------
// DX evaluator trait (decouples pancetta-qso from pancetta-dx)
// ---------------------------------------------------------------------------

/// Trait for scoring how interesting a CQ call is.
///
/// Implemented by a thin adapter wrapping `pancetta-dx::PriorityManager` + `RarityScorer`
/// in the coordinator wiring layer.
pub trait DxEvaluator: Send + Sync {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64;

    /// Tiered classification, when available. `None` for evaluators that
    /// don't implement tiered scoring (e.g. `NullDxEvaluator`, test
    /// scaffolding) — callers that need a tier (the DX watchlist) simply
    /// skip entries where this returns `None`.
    fn evaluate_cq_tiered(
        &self,
        _callsign: &str,
        _grid: Option<&str>,
        _snr: i8,
        _freq_hz: f64,
    ) -> Option<crate::priority::TieredScore> {
        None
    }
}

/// A no-op evaluator that assigns the same score to everything.
#[derive(Debug, Clone)]
pub struct NullDxEvaluator;

impl DxEvaluator for NullDxEvaluator {
    fn evaluate_cq(&self, _callsign: &str, _grid: Option<&str>, _snr: i8, _freq_hz: f64) -> f64 {
        0.5
    }
}

// ---------------------------------------------------------------------------
// CQ candidate (a CQ heard on the last RX slot)
// ---------------------------------------------------------------------------

/// A CQ we decoded during the most recent RX slot.
#[derive(Debug, Clone)]
pub struct CqCandidate {
    pub callsign: String,
    pub grid: Option<String>,
    pub snr: i8,
    pub frequency_hz: f64,
    pub dx_score: f64,
    /// The parity of the slot in which this CQ was heard.
    /// Used to derive `tx_parity = slot_parity.opposite()` for our response.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
    /// hb-103 (Batch 32): original message text the CQ was parsed from.
    /// Used as input to the content-score TX gate.
    pub message_text: String,
    /// hb-103 (Batch 32): decoder confidence in `[0, 1]`. `None` for
    /// pre-hb-103 code paths.
    pub confidence: Option<f32>,
    /// hb-103 (Batch 32): time offset of the decode within its slot.
    /// `None` for pre-hb-103 code paths.
    pub time_offset_s: Option<f64>,
    /// hb-247 (Batch 81): decode-origin ordinal carried from the
    /// originating `DecodedMessageInfo`; v3 content-score input.
    pub decode_origin: Option<u8>,
}

// ---------------------------------------------------------------------------
// Operating states and operator actions
// ---------------------------------------------------------------------------

/// High-level operating state of the autonomous operator.
#[derive(Debug, Clone, PartialEq)]
pub enum OperatingState {
    /// Listening for interesting CQs to respond to.
    Hunting,
    /// Calling CQ ourselves.
    CallingCq,
    /// Actively in one or more QSOs.
    InQso { qso_count: u32 },
    /// Listening on our TX slot for collision detection.
    ListeningForCollisions,
    /// Operator paused by user.
    Paused,
}

impl std::fmt::Display for OperatingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperatingState::Hunting => write!(f, "Hunting"),
            OperatingState::CallingCq => write!(f, "Calling CQ"),
            OperatingState::InQso { qso_count } => write!(f, "In QSO ({})", qso_count),
            OperatingState::ListeningForCollisions => write!(f, "Collision Listen"),
            OperatingState::Paused => write!(f, "Paused"),
        }
    }
}

/// Actions the autonomous operator emits each cycle.
#[derive(Debug, Clone)]
pub enum OperatorAction {
    /// Transmit an FT8 message at the given offset.
    Transmit {
        message_text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        /// Required slot parity. `None` = no DX context (CQ or follow-up
        /// without a latched heard-slot); the TX scheduler falls back to
        /// the configured self-parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Listen (do not transmit this slot).
    Listen,
    /// Listen specifically for collisions on our TX offset.
    CollisionListen,
    /// Request a band/frequency change via Hamlib.
    ChangeBand { dial_frequency: u64 },
    /// Shift our TX offset (collision avoidance).
    FrequencyShift { new_offset_hz: f64 },
    /// Status update for TUI consumption.
    StatusUpdate(AutonomousStatusData),
}

/// Why the autonomous operator did not act on a decoded CQ this slot.
/// Bookkeeping only: this never influences the returned actions.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    AtCapacity { active: u32, cap: u32 },
    DxBusy { window_secs: u64 },
    RecentlyResponded,
    CallsignContinuity { dx_score: f64 },
    ContentScore { score: f64, threshold: f64 },
    FrequencyClash,
}

/// A CQ candidate rejected by an autonomous selection filter.
#[derive(Debug, Clone, PartialEq)]
pub struct CqSkipRecord {
    pub callsign: Option<String>,
    pub reason: SkipReason,
}

/// Hard per-slot bound for CQ skip bookkeeping.
pub const MAX_SKIP_LOG_PER_SLOT: usize = 32;

/// Status data sent to the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousStatusData {
    pub enabled: bool,
    pub state: String,
    pub slot_parity: Option<String>,
    pub listen_counter: String,
    pub active_qsos: u32,
    pub max_qsos: u32,
    pub idle_cycles: u32,
    pub band_name: String,
    pub tx_offset_hz: f64,
}

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

/// Configuration for the listen-cycle adaptive policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenCycleConfig {
    /// How many of our TX cycles between forced listens (initial).
    pub initial_interval: u32,
    /// Back-off interval after enough clean listens.
    pub backoff_interval: u32,
    /// Interval used when a collision has been detected recently.
    pub collision_interval: u32,
    /// Number of clean listens before back-off kicks in.
    pub backoff_threshold: u32,
}

impl Default for ListenCycleConfig {
    fn default() -> Self {
        Self {
            initial_interval: 3,
            backoff_interval: 5,
            collision_interval: 2,
            backoff_threshold: 5,
        }
    }
}

/// Band hopping entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandEntry {
    pub dial_frequency: u64,
    pub band_name: String,
    pub priority: u32,
}

/// Band hopping configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandHoppingConfig {
    pub enabled: bool,
    /// Number of low-activity cycles before hopping.
    pub hop_threshold: u32,
    pub bands: Vec<BandEntry>,
}

impl Default for BandHoppingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hop_threshold: 20,
            bands: vec![
                BandEntry {
                    dial_frequency: 14_074_000,
                    band_name: "20m".into(),
                    priority: 1,
                },
                BandEntry {
                    dial_frequency: 7_074_000,
                    band_name: "40m".into(),
                    priority: 2,
                },
            ],
        }
    }
}

/// Top-level autonomous operator configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousConfig {
    pub enabled: bool,
    pub slot_parity: SlotParityConfig,
    /// Idle TX cycles before we start calling CQ ourselves.
    pub cq_after_idle_cycles: u32,
    /// Consecutive self-CQ transmissions with zero decoded responses before
    /// switching to a different TX frequency (Auto mode only).
    pub cq_no_response_switch_after: u32,
    pub max_concurrent_qsos: u32,
    pub tx_offset_hz: f64,
    /// 0.0–1.0 threshold for DX score when deciding whether to answer a CQ.
    pub min_dx_score: f64,
    /// Minimum DX score required to open an additional QSO slot (0.0–1.0).
    /// Only applies to second+ concurrent QSOs. First QSO uses min_dx_score.
    pub min_multi_slot_score: f64,
    /// Directed CQ text (e.g. "DX", "NA", or empty).
    pub cq_direction: String,
    pub listen_cycle: ListenCycleConfig,
    pub band_hopping: BandHoppingConfig,
    /// Frequency allocator settings for smart TX offset selection.
    pub frequency: FrequencyAllocatorConfig,
    /// DX-busy suppression window (seconds). If a DX station was seen
    /// participating in a non-CQ exchange (report / RR73 / 73 not directed
    /// at us) within this window, the autonomous operator will not start a
    /// new call to it even if it briefly CQs again — it is presumed busy
    /// with a third party. Default: 90 s.
    pub dx_busy_window_secs: u64,
    /// DX watchlist (#197) TTL in seconds: how long a `PerBandDxccNew`+/
    /// `Atno` CQ heard-but-not-pounced-on stays remembered before being
    /// dropped as presumed moved on. Default: 150 s (~2.5 min).
    pub watchlist_ttl_secs: u64,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slot_parity: SlotParityConfig::Auto,
            cq_after_idle_cycles: 10,
            cq_no_response_switch_after: 5,
            max_concurrent_qsos: 1,
            tx_offset_hz: 1500.0,
            min_dx_score: 0.3,
            min_multi_slot_score: 0.7,
            cq_direction: String::new(),
            listen_cycle: ListenCycleConfig::default(),
            band_hopping: BandHoppingConfig::default(),
            frequency: FrequencyAllocatorConfig::default(),
            dx_busy_window_secs: 90,
            watchlist_ttl_secs: 150,
        }
    }
}

// ---------------------------------------------------------------------------
// Band strategy (Phase 6)
// ---------------------------------------------------------------------------

/// Tracks per-band activity and decides when to hop.
#[derive(Debug, Clone)]
pub struct BandStrategy {
    config: BandHoppingConfig,
    current_band_index: usize,
    low_activity_cycles: u32,
    /// Settling period after a band change — don't make decisions.
    settling_cycles: u32,
    activity_per_band: HashMap<String, u32>,
}

impl BandStrategy {
    pub fn new(config: BandHoppingConfig) -> Self {
        let activity_per_band = config
            .bands
            .iter()
            .map(|b| (b.band_name.clone(), 0))
            .collect();

        Self {
            config,
            current_band_index: 0,
            low_activity_cycles: 0,
            settling_cycles: 0,
            activity_per_band,
        }
    }

    /// Record decoded message count for the current cycle.
    pub fn record_activity(&mut self, decoded_count: u32) {
        if self.settling_cycles > 0 {
            self.settling_cycles -= 1;
            return;
        }

        if decoded_count == 0 {
            self.low_activity_cycles += 1;
        } else {
            self.low_activity_cycles = 0;
        }

        if let Some(band) = self.config.bands.get(self.current_band_index) {
            *self
                .activity_per_band
                .entry(band.band_name.clone())
                .or_insert(0) += decoded_count;
        }
    }

    /// Check if we should hop. Returns the new dial frequency if so.
    pub fn should_hop(&mut self) -> Option<u64> {
        if !self.config.enabled || self.settling_cycles > 0 {
            return None;
        }

        if self.low_activity_cycles >= self.config.hop_threshold && self.config.bands.len() > 1 {
            // Move to next band in priority order.
            self.current_band_index = (self.current_band_index + 1) % self.config.bands.len();
            self.low_activity_cycles = 0;
            self.settling_cycles = 2; // 2-cycle settling period.

            let band = &self.config.bands[self.current_band_index];
            info!(
                "Band hopping to {} ({})",
                band.band_name, band.dial_frequency
            );
            Some(band.dial_frequency)
        } else {
            None
        }
    }

    pub fn current_band_name(&self) -> &str {
        self.config
            .bands
            .get(self.current_band_index)
            .map(|b| b.band_name.as_str())
            .unwrap_or("Unknown")
    }
}

// ---------------------------------------------------------------------------
// Frequency allocator (multi-QSO support)
// ---------------------------------------------------------------------------

/// Manages frequency allocation for concurrent QSOs.
///
/// Tracks in-use frequencies (own QSOs + decoded signals) and allocates
/// clear frequencies for new transmissions with minimum separation.
#[derive(Debug, Clone)]
pub struct FrequencyAllocator {
    /// Frequencies currently in use by our own QSOs (offset_hz → qso_id).
    own_frequencies: HashMap<String, f64>,
    /// Frequencies seen in the last RX window (from decoded messages).
    observed_frequencies: Vec<f64>,
    /// Minimum separation between our own TX signals (Hz).
    min_separation_hz: f64,
    /// Frequency range for allocation (min, max) in Hz offset.
    allocation_range: (f64, f64),
}

impl FrequencyAllocator {
    pub fn new(min_separation_hz: f64, allocation_range: (f64, f64)) -> Self {
        Self {
            own_frequencies: HashMap::new(),
            observed_frequencies: Vec::new(),
            min_separation_hz,
            allocation_range,
        }
    }

    /// Update observed frequencies from the latest decode window.
    pub fn update_observed(&mut self, decoded: &[DecodedMessageInfo]) {
        self.observed_frequencies = decoded.iter().map(|m| m.frequency_hz).collect();
    }

    /// Register a frequency as in use by one of our QSOs.
    pub fn register_qso_frequency(&mut self, qso_id: &str, frequency_hz: f64) {
        self.own_frequencies
            .insert(qso_id.to_string(), frequency_hz);
    }

    /// Remove a QSO's frequency allocation.
    pub fn release_qso_frequency(&mut self, qso_id: &str) {
        self.own_frequencies.remove(qso_id);
    }

    /// FQ-F3: wholesale-replace the own-frequency registry from a fresh
    /// snapshot (qso_id -> TX offset Hz).
    ///
    /// This is the production entry point the coordinator's autonomous slot
    /// loop calls each tick with a snapshot of `active_tx_offsets` (see
    /// `pancetta/src/coordinator/autonomous.rs`) — a bulk replace rather
    /// than diffing against the previous tick and calling
    /// `register_qso_frequency`/`release_qso_frequency` for the deltas.
    /// Bulk-replace was chosen over diffing because it can never leak a
    /// stale entry: if the coordinator-side map and this registry ever
    /// drift out of sync for any reason (a missed event, a task restart,
    /// races between insert/remove call sites), the very next tick's
    /// wholesale replace self-heals it. A diff-based approach would need
    /// its own separate bookkeeping of "what did we register last tick"
    /// and any bug in THAT bookkeeping could leave a released QSO's
    /// frequency registered forever.
    pub fn set_own_frequencies(&mut self, frequencies: HashMap<String, f64>) {
        self.own_frequencies = frequencies;
    }

    /// Check if a frequency is clear of our own TX signals.
    pub fn is_clear_of_own(&self, frequency_hz: f64) -> bool {
        self.own_frequencies
            .values()
            .all(|&f| (f - frequency_hz).abs() >= self.min_separation_hz)
    }

    /// Check if a frequency is reasonably clear of observed activity.
    /// Uses a smaller tolerance since we want to reply on the caller's frequency.
    pub fn is_clear_of_observed(&self, frequency_hz: f64, tolerance_hz: f64) -> bool {
        self.observed_frequencies
            .iter()
            .filter(|&&f| (f - frequency_hz).abs() < tolerance_hz)
            .count()
            <= 1 // Allow the station we're replying to
    }

    /// Find a clear frequency for a new CQ, avoiding own QSOs and busy areas.
    pub fn allocate_cq_frequency(&self) -> f64 {
        let (min_f, max_f) = self.allocation_range;
        let step = self.min_separation_hz;

        // Try candidates from the middle outward
        let center = (min_f + max_f) / 2.0;
        let mut best = center;
        let mut best_clearance = f64::NEG_INFINITY;

        let mut freq = min_f;
        while freq <= max_f {
            let min_dist_own = self
                .own_frequencies
                .values()
                .map(|&f| (f - freq).abs())
                .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(f64::MAX);

            let nearby_count = self
                .observed_frequencies
                .iter()
                .filter(|&&f| (f - freq).abs() < 100.0)
                .count();

            // Clearance score: distance from own QSOs, penalize busy areas
            let clearance = min_dist_own - (nearby_count as f64 * 20.0);
            if clearance > best_clearance {
                best_clearance = clearance;
                best = freq;
            }

            freq += step;
        }

        best.clamp(min_f, max_f)
    }

    /// Get all own frequencies currently allocated.
    pub fn own_frequencies(&self) -> &HashMap<String, f64> {
        &self.own_frequencies
    }
}

// ---------------------------------------------------------------------------
// The autonomous operator itself
// ---------------------------------------------------------------------------

// Per-callsign rate-limit window, in seconds. If we initiated a response
// to a callsign within this duration we skip re-initiating. Defends
// against an attacker spamming `CQ FAKECALL FN42` every cycle to flood
// pancetta's QSO slots and log. (Security review 2026-04-29 I-1.)
//
// Stored as a plain `i64` of seconds so the windows can be evaluated
// against the injectable [`DateTime<Utc>`] clock the sim harness drives
// (mirrors `QsoManager`'s `check_timeouts_at(now)` pattern). The
// time-dependent maps below key on `DateTime<Utc>` for the same reason.
const RECENT_RESPONSE_WINDOW_SECS: i64 = 60;

/// Snapshot of the fields a `decide_at` self-CQ cycle may mutate
/// speculatively (streak, sticky offset, config offset). See
/// [`AutonomousOperator::snapshot_cq_state`]/[`AutonomousOperator::restore_cq_state`].
#[derive(Debug, Clone, Copy)]
pub struct CqStateSnapshot {
    streak: u32,
    current_cq_offset_hz: Option<f64>,
    tx_offset_hz: f64,
    /// PAN-38: identifies which self-CQ attempt this snapshot belongs to.
    /// Monotonically assigned by [`AutonomousOperator::decide_at`] every
    /// time it takes a fresh snapshot. Round-trips through the coordinator
    /// on `StartAutonomousQso` and back on a downstream dispatch-failure
    /// signal, so [`AutonomousOperator::restore_cq_state_for_attempt`] can
    /// tell a late failure report for an OLD attempt apart from a newer
    /// self-CQ that has since overwritten `last_cq_snapshot` — restoring
    /// the wrong one would silently corrupt the newer attempt's state.
    attempt_id: u64,
    /// PAN-38 round 1 (Codex): whether THIS attempt's own dispatch performed
    /// a threshold-driven frequency switch (reset the streak to a fresh
    /// baseline and picked a new offset), set immediately after that
    /// decision runs in `decide_at`. Lets a bounded compensating rollback
    /// (see [`AutonomousOperator::restore_cq_state_for_attempt`]) tell a
    /// routine failed attempt (safe to compensate: just decrement the
    /// streak by the one increment it contributed) from a switching one
    /// (unsafe to auto-compensate once superseded: undoing a switch means
    /// restoring an absolute pre-attempt baseline, which would corrupt
    /// whatever legitimately happened after it).
    did_switch: bool,
    /// PAN-38 round 4 (Codex): `AutonomousOperator::offset_generation` as
    /// observed at snapshot time. Bumped by every event that invalidates
    /// `current_cq_offset_hz`/`config.tx_offset_hz` OUTSIDE this snapshot's
    /// own speculative mutations -- a Hold/Auto transition (both the direct
    /// and the between-polls generation-mirrored case, PAN-39), a band hop,
    /// or the offset half of a collision jitter. A restore only reinstates
    /// the offset/config fields when this still matches the CURRENT
    /// generation. Split from the streak generation below (round 3 gated
    /// both on one shared counter, which over-blocked a safe streak
    /// restore whenever only the offset had been invalidated).
    offset_generation: u32,
    /// PAN-38 round 4 (Codex): `AutonomousOperator::streak_generation` as
    /// observed at snapshot time. Bumped by every event that invalidates
    /// `cq_no_response_streak` OUTSIDE this snapshot's own speculative
    /// mutations -- a genuine directed-reply reset, or the streak half of a
    /// collision jitter. A restore only reinstates the streak when this
    /// still matches the CURRENT generation. See
    /// [`AutonomousOperator::restore_cq_state`] and
    /// [`AutonomousOperator::restore_cq_state_for_attempt`].
    streak_generation: u32,
}

/// The per-cycle decision-making brain.
///
/// Each TX slot it runs a decision tree:
/// 1. Slot manager → Listen / NotOurSlot / Transmit
/// 2. If Transmit: active QSOs? → delegate to auto_sequencer
/// 3. No active QSOs: any interesting CQs from last RX? → respond
/// 4. Nothing interesting: idle long enough? → CQ
/// 5. Otherwise: idle++, listen
pub struct AutonomousOperator {
    config: AutonomousConfig,
    slot_manager: SlotManager,
    collision_detector: CollisionDetector,
    band_strategy: BandStrategy,
    frequency_allocator: FrequencyAllocator,
    state: OperatingState,
    idle_cycles: u32,
    /// Consecutive self-CQ transmissions with zero decoded responses. Reset
    /// by `feed_decoded_messages_at` whenever a decoded message directs a
    /// reply at our callsign (see [`is_directed_response`]) — deliberately
    /// NOT keyed off `active_qso_count`, since our own self-CQ opens a
    /// `CallingCq` QSO that is itself "active" until it times out, which
    /// would otherwise reset the streak against our own unanswered call.
    /// Tracked regardless of `TxFreqMode` (cheap, and ready if the operator
    /// switches to Auto — same precedent as `QsoMetadata.dx_repeat_count`);
    /// only acted on in Auto mode.
    cq_no_response_streak: u32,
    /// The offset actually used for the most recent self-CQ (Auto mode
    /// only). Sticky: a routine (non-switching) self-CQ reuses this value
    /// directly instead of re-ranking from scratch, so a threshold-driven
    /// switch's new offset remains the baseline for the next streak rather
    /// than being discarded after one transmission. Also the correct
    /// "offset to avoid" when a further switch is warranted — unlike
    /// `config.tx_offset_hz`, which the routine path does not otherwise keep
    /// in sync (e.g. the live-spot rarity boost can pick something else).
    /// Cleared on a band hop, since it's scoped to the old band's audio
    /// offset space.
    current_cq_offset_hz: Option<f64>,
    /// Snapshot of state taken immediately before the current cycle's
    /// speculative self-CQ mutations, if `decide_at` reached that point this
    /// cycle. The coordinator calls [`Self::restore_cq_state`] to pop and
    /// apply it when a downstream gate suppressed the self-CQ before it
    /// reached the radio. `None` on any cycle that didn't attempt a self-CQ.
    last_cq_snapshot: Option<CqStateSnapshot>,
    /// PAN-38 round 1 (Codex): the PREVIOUS unresolved snapshot, kept for
    /// exactly one more generation so a downstream failure report delayed
    /// until a NEWER self-CQ attempt has already overwritten
    /// `last_cq_snapshot` can still be bounded-compensated (see
    /// [`Self::restore_cq_state_for_attempt`]) instead of silently dropped.
    /// Shifted in from `last_cq_snapshot` whenever a new snapshot is taken
    /// over a still-unresolved one; cleared whenever either snapshot is
    /// consumed. Bounded to one generation deep — a failure report stale by
    /// more than that is logged but not auto-corrected (see that method's
    /// doc comment for why going further isn't safely boundable).
    previous_cq_snapshot: Option<CqStateSnapshot>,
    /// PAN-38: monotonic counter, incremented every time `decide_at` takes a
    /// new `last_cq_snapshot`. The current value becomes that snapshot's
    /// `CqStateSnapshot::attempt_id`.
    cq_attempt_counter: u64,
    /// Real FT8 message parser, used by [`Self::is_directed_response`] to
    /// recognize a genuine reply (handles compound/hash-rendered callsigns
    /// and every valid rung shape correctly, instead of a hand-rolled text
    /// heuristic).
    exchange: MessageExchange,
    our_callsign: String,
    our_grid: Option<String>,
    /// CQs decoded in the most recent RX slot.
    pending_cqs: Vec<CqCandidate>,
    /// Number of active QSOs (tracked externally, fed in).
    active_qso_count: u32,
    /// Messages to transmit from the auto-sequencer (fed in).
    /// Each entry: (message_text, frequency_offset, qso_id).
    pending_sequencer_messages: Vec<(String, f64, Option<String>)>,
    /// Rolling buffer of recent decode activity for frequency allocation.
    decode_history: DecodeHistory,
    /// Latest spectral snapshot from the waterfall data.
    spectral_snapshot: Option<SpectralSnapshot>,
    /// Smart frequency allocator (replaces simple FrequencyAllocator for new QSOs).
    smart_allocator: SmartFrequencyAllocator,
    /// Whether the user has paused autonomous operation.
    paused: bool,
    /// Live spot frequencies for frequency nudging: (frequency_hz, rarity 0.0-1.0)
    live_spot_frequencies: Vec<(f64, f64)>,
    /// Per-callsign rate limit: callsign → time we last initiated a
    /// response. Skips re-initiating to the same callsign within
    /// RECENT_RESPONSE_WINDOW. Defends against an attacker spamming `CQ
    /// FAKECALL FN42` every cycle to flood pancetta's QSO slots and
    /// log. (Security review 2026-04-29 I-1.)
    ///
    /// Keyed on `DateTime<Utc>` (not `Instant`) so the window can be
    /// evaluated against the sim harness's injectable virtual clock.
    recently_responded_to: HashMap<String, DateTime<Utc>>,
    /// DX-busy tracker: callsign → time it was last seen participating in
    /// a non-CQ exchange (report / RR73 / 73) that was NOT directed at us
    /// and was NOT a CQ. If a station appears here within
    /// `config.dx_busy_window_secs`, the autonomous operator presumes it is
    /// working a third party and suppresses any auto-response to its CQ.
    /// Populated from `feed_decoded_messages`.
    ///
    /// Keyed on `DateTime<Utc>` (not `Instant`) so the busy window can be
    /// evaluated against the sim harness's injectable virtual clock.
    recently_in_qso: HashMap<String, DateTime<Utc>>,
    /// Phase-5 hardening #1: optional callsign-continuity FP filter
    /// consulted before responding to any CQ. Defends against OSD
    /// fabrications (`R44XYB`, `OR1QRD`, ...) reaching the TX path.
    /// When `None` (default; constructed via `new`), all CQs are
    /// allowed through — the decode-side filter still runs in the
    /// coordinator, this is a second-layer TX gate.
    fp_filter: Option<std::sync::Arc<crate::callsign_continuity::CallsignContinuityFilter>>,
    /// Operator TX-frequency mode (`pancetta_core::TxFreqMode` as `u8`), shared
    /// from the coordinator. In the default `Hold` mode we never pick or move
    /// the TX offset on our own: the smart-frequency allocator falls back to
    /// the operator's pinned `config.tx_offset_hz`, and the collision-listen
    /// jitter is suppressed. `Auto` re-enables both. Defaults to a private
    /// `Hold` atomic so any caller that never injects a source holds frequency.
    tx_freq_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Bumped by the coordinator every time it stores a new value into
    /// `tx_freq_mode` (i.e. once per Hold/Auto transition), shared from the
    /// coordinator. `decide_at` compares this against the generation it last
    /// observed to detect a Hold entry+exit that happened entirely between
    /// two polling cycles — a plain "is the mode Hold right now" read of the
    /// mode atomic can't see that, since by the time `decide_at` next polls,
    /// the mode is back to Auto. A generation counter catches it without
    /// needing a direct handle from `tui_relay.rs`'s command handlers into
    /// this operator (which lives behind its own `Arc<Mutex<..>>` elsewhere
    /// in the coordinator). Defaults to a private counter that's never
    /// bumped, matching `tx_freq_mode`'s own "unwired → today's behavior"
    /// convention.
    tx_freq_mode_generation: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// The generation value `decide_at` last observed. Any mismatch against
    /// the live `tx_freq_mode_generation` means at least one Hold/Auto
    /// transition happened since — the sticky offset is invalidated
    /// regardless of what the CURRENT mode value reads as.
    last_seen_tx_freq_mode_generation: u32,
    /// PAN-38 round 4 (Codex): a plain (non-atomic — only ever touched while
    /// this operator's own lock is held) counter bumped by every event that
    /// invalidates `current_cq_offset_hz`/`config.tx_offset_hz` OUTSIDE of
    /// `decide_at`'s own speculative self-CQ mutations: a Hold/Auto
    /// transition (both the direct "mode reads Hold right now" case and the
    /// generation-mirrored "squeezed entirely between two polls" case), a
    /// band hop, or the offset half of a collision jitter.
    /// `restore_cq_state`/`restore_cq_state_for_attempt` compare a
    /// snapshot's stamped offset generation against this CURRENT value —
    /// any mismatch means the snapshot's offset/config fields no longer
    /// describe a state that's safe to restore. Split from
    /// `streak_generation` below (round 3 gated both under one shared
    /// counter, which over-blocked a safe streak restore whenever only the
    /// offset had been invalidated).
    offset_generation: u32,
    /// PAN-38 round 4 (Codex): the streak counterpart to `offset_generation`
    /// above — bumped by every event that invalidates
    /// `cq_no_response_streak` OUTSIDE of `decide_at`'s own speculative
    /// self-CQ mutations: a genuine directed-reply reset, or the streak
    /// half of a collision jitter. Compared independently so a
    /// pure-offset invalidation (Hold/Auto, band hop) no longer blocks a
    /// safe streak restore/compensation, and vice versa.
    streak_generation: u32,
    /// Operator's LIVE parked TX offset (Hz), shared from the coordinator's
    /// `tx_offset_hold_hz` atomic (the same one the `o` modal writes via
    /// `TuiCommand::SetTxOffset`). `0` is the "unset/unparked" sentinel —
    /// same convention as `coordinator/autonomous.rs`'s `parked_bin_coverage`
    /// and `coordinator/qso.rs`'s `compute_manual_tx_offset`. When `None`
    /// (never wired, e.g. in unit tests) or when the atomic reads `0`,
    /// Hold-mode falls back to the static `config.tx_offset_hz` — today's
    /// behavior is preserved byte-for-byte.
    tx_offset_hold_hz: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// DX watchlist (#197): short-lived memory of `PerBandDxccNew`+/`Atno`
    /// CQs heard but not pounced on. See `pancetta_qso::watchlist`.
    watchlist: DxWatchlist,
    /// Rejections recorded during the most recent decision cycle.
    skip_log: Vec<CqSkipRecord>,
}

impl AutonomousOperator {
    pub fn new(config: AutonomousConfig, our_callsign: String, our_grid: Option<String>) -> Self {
        let slot_manager = SlotManager::new(config.slot_parity, &config.listen_cycle);
        let collision_detector = CollisionDetector::new(config.tx_offset_hz, 50.0);
        let band_strategy = BandStrategy::new(config.band_hopping.clone());
        // FT8 bandwidth: 8 tones * 6.25 Hz = 50 Hz, plus 25 Hz guard = 75 Hz min separation
        let frequency_allocator = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        let decode_history = DecodeHistory::new(config.frequency.decode_history_cycles);
        let smart_allocator = SmartFrequencyAllocator::new(config.frequency.clone());
        let watchlist =
            DxWatchlist::new(chrono::Duration::seconds(config.watchlist_ttl_secs as i64));
        let exchange = MessageExchange::new(our_callsign.clone());

        Self {
            config,
            slot_manager,
            collision_detector,
            band_strategy,
            frequency_allocator,
            state: OperatingState::Hunting,
            idle_cycles: 0,
            cq_no_response_streak: 0,
            current_cq_offset_hz: None,
            last_cq_snapshot: None,
            previous_cq_snapshot: None,
            cq_attempt_counter: 0,
            exchange,
            our_callsign,
            our_grid,
            pending_cqs: Vec::new(),
            active_qso_count: 0,
            pending_sequencer_messages: Vec::new(),
            decode_history,
            spectral_snapshot: None,
            smart_allocator,
            paused: false,
            live_spot_frequencies: Vec::new(),
            recently_responded_to: HashMap::new(),
            recently_in_qso: HashMap::new(),
            fp_filter: None,
            tx_freq_mode: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Hold.as_u8(),
            )),
            tx_freq_mode_generation: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_seen_tx_freq_mode_generation: 0,
            offset_generation: 0,
            streak_generation: 0,
            tx_offset_hold_hz: None,
            watchlist,
            skip_log: Vec::new(),
        }
    }

    /// Drain CQ skips recorded by the most recent decision cycle.
    pub fn take_skip_log(&mut self) -> Vec<CqSkipRecord> {
        std::mem::take(&mut self.skip_log)
    }

    /// Undo this cycle's speculative self-CQ mutations — call when the
    /// coordinator determines the self-CQ `decide_at` emitted was suppressed
    /// by a downstream gate (Shift+Q runtime gate, TX policy, operator-
    /// presence/FCC §97.221 gate, dry_run) before reaching the radio.
    ///
    /// No-op if this cycle didn't reach the self-CQ branch (`decide_at`
    /// clears `last_cq_snapshot` back to `None` after consuming it here, so
    /// calling this twice, or on a cycle with no self-CQ, is always safe).
    ///
    /// Codex review (PR #276, rounds 2-4): `decide_at` mutates
    /// `cq_no_response_streak`, `current_cq_offset_hz`, and (on a
    /// threshold-driven switch) `config.tx_offset_hz` speculatively. A
    /// simple "subtract one from the streak" undo is wrong for a suppressed
    /// switch (round 3 — misses the offset/config changes a switch also
    /// makes). The snapshot itself must also be taken from INSIDE
    /// `decide_at`, immediately before those mutations, not by the
    /// coordinator before calling `decide()` (round 4 — a pre-`decide()`
    /// snapshot would "restore" a value Step 0's band-hop/Hold-mode
    /// handling had *just correctly invalidated* earlier in the very same
    /// cycle, e.g. `current_cq_offset_hz` cleared on a same-cycle band hop).
    ///
    /// PAN-38 round 3 (Codex): a restore of a given field is only safe when
    /// its generation still matches what it was at snapshot time -- i.e.
    /// nothing has touched THAT field since. A mismatch means the
    /// snapshot's value for that field no longer describes a state that's
    /// safe to restore (e.g. a genuine directed reply reset the streak to
    /// 0; blindly restoring the pre-attempt streak would silently discard
    /// that reset).
    ///
    /// PAN-38 round 4 (Codex): the streak and offset/config fields are
    /// gated INDEPENDENTLY (`streak_generation` vs `offset_generation`),
    /// not behind one shared flag -- round 3's single combined generation
    /// over-blocked a safe streak restore whenever only the offset had
    /// been invalidated (e.g. a Hold/Auto transition or band hop, neither
    /// of which touches the streak), permanently under-counting a
    /// suppressed attempt's contribution in that case.
    ///
    /// PAN-38 round 5 (Codex): `offset_generation` is only advanced by
    /// `refresh_tx_freq_offset_invalidation` -- which normally runs once
    /// per `decide_at` poll. If the operator changes the TX-frequency
    /// mode (or held offset) AFTER a snapshot was taken and a downstream
    /// failure arrives BEFORE the next poll, `offset_generation` hasn't
    /// caught up with the live `tx_freq_mode_generation` atomic yet, so
    /// the check below would wrongly still match and restore a now-stale
    /// offset/config/collision-detector state. Refresh first so this
    /// restore is always evaluated against the CURRENT live state, not
    /// whatever this operator last happened to poll.
    pub fn restore_cq_state(&mut self) {
        self.refresh_tx_freq_offset_invalidation();
        let Some(snapshot) = self.last_cq_snapshot.take() else {
            return;
        };
        if self.streak_generation == snapshot.streak_generation {
            self.cq_no_response_streak = snapshot.streak;
        } else {
            debug!(
                "Autonomous self-CQ attempt {} suppressed but the streak was independently \
                 invalidated (directed reply or collision jitter) after the snapshot was \
                 taken -- streak restore skipped to avoid resurrecting stale state",
                snapshot.attempt_id
            );
        }
        if self.offset_generation == snapshot.offset_generation {
            self.current_cq_offset_hz = snapshot.current_cq_offset_hz;
            self.config.tx_offset_hz = snapshot.tx_offset_hz;
            self.collision_detector.our_tx_offset_hz = snapshot.tx_offset_hz;
        } else {
            debug!(
                "Autonomous self-CQ attempt {} suppressed but the offset was independently \
                 invalidated (Hold/Auto transition or band hop) after the snapshot was taken \
                 -- offset/config restore skipped to avoid resurrecting stale state",
                snapshot.attempt_id
            );
        }
    }

    /// The `attempt_id` of the most recent unresolved self-CQ snapshot, if
    /// `decide_at` reached the self-CQ branch this cycle and the snapshot
    /// hasn't since been consumed by [`Self::restore_cq_state`]. The
    /// coordinator reads this right after `decide()`/`decide_at()` returns
    /// (before any suppression check may consume it) so it can tag a
    /// dispatched self-CQ with the attempt it belongs to — see
    /// [`Self::restore_cq_state_for_attempt`].
    pub fn last_cq_attempt_id(&self) -> Option<u64> {
        self.last_cq_snapshot.map(|s| s.attempt_id)
    }

    /// PAN-38: undo a self-CQ's speculative mutations after it was dispatched
    /// (survived every pre-dispatch gate) but then failed downstream — e.g. a
    /// radio/CAT error or a subsystem race in `QsoManager::start_cq` — with
    /// no QSO ever actually opened. The coordinator calls this from the
    /// failure signal handler, passing back the `attempt_id` it received on
    /// `StartAutonomousQso`.
    ///
    /// The common case reuses [`Self::restore_cq_state`] directly: the
    /// failed attempt is still the LATEST snapshot, so a full restore to its
    /// pre-attempt values is exactly correct.
    ///
    /// PAN-38 round 1 (Codex): a failure report delayed until a LATER
    /// self-CQ attempt has already taken its own (newer) snapshot used to be
    /// a silent no-op — permanently baking the failed attempt's speculative
    /// "+1 streak" (and, if it switched, its offset change) into the newer
    /// attempt's baseline. `previous_cq_snapshot` keeps exactly one extra
    /// generation of history so this case can still be bounded-compensated:
    /// - If the stale attempt did NOT itself trigger a frequency switch
    ///   (`did_switch == false`), its only speculative effect was the simple
    ///   `+1` to `cq_no_response_streak` every dispatch applies. Decrementing
    ///   the CURRENT streak by 1 exactly undoes it PROVIDED `cq_state_
    ///   generation` still matches this snapshot's -- i.e. nothing else
    ///   (a directed-reply reset, collision jitter, a mode transition, a
    ///   band hop) has touched the streak/offset since. A mismatch means the
    ///   streak is no longer purely additive across that gap (PAN-38 round 3,
    ///   Codex: e.g. a genuine directed reply zeroed it in between -- a
    ///   blind `-1` would then wrongly eat into the newer reset instead of
    ///   the failed attempt's own contribution), so the correction is
    ///   skipped and logged instead of guessed.
    /// - If the stale attempt DID switch, undoing it means restoring an
    ///   ABSOLUTE pre-attempt baseline, which is only safe if nothing has
    ///   changed since (i.e. it's still the effectively-latest switch). That
    ///   additional bookkeeping isn't tracked — auto-correcting a stale
    ///   *switching* attempt risks silently discarding a legitimate later
    ///   switch, worse than the gap it would close. Logged instead, so the
    ///   (rarer still) case at least has visibility instead of silent drift.
    /// - A failure stale by MORE than one generation (evicted from both
    ///   slots) is unchanged from before this round: logged, not corrected.
    pub fn restore_cq_state_for_attempt(&mut self, attempt_id: u64) {
        // PAN-38 round 5: refresh unconditionally, before either branch --
        // see `restore_cq_state`'s doc comment for why a stale internal
        // generation (relative to the LIVE tx_freq_mode_generation atomic)
        // is unsafe to evaluate a restore against.
        self.refresh_tx_freq_offset_invalidation();
        if self
            .last_cq_snapshot
            .is_some_and(|s| s.attempt_id == attempt_id)
        {
            self.restore_cq_state();
            return;
        }
        if let Some(snapshot) = self.previous_cq_snapshot {
            if snapshot.attempt_id == attempt_id {
                self.previous_cq_snapshot = None;
                if snapshot.did_switch {
                    warn!(
                        "Autonomous self-CQ attempt {} failed downstream after a newer attempt \
                         already superseded it, and it performed a frequency switch -- streak/\
                         offset NOT auto-corrected (would risk discarding a legitimate later \
                         switch); state may be off by one attempt",
                        attempt_id
                    );
                } else if self.streak_generation == snapshot.streak_generation {
                    // Only the streak generation matters here: a
                    // non-switching attempt never touched the offset, so an
                    // offset-only invalidation (Hold/Auto, band hop) since
                    // this snapshot has no bearing on whether the streak
                    // -1 is still correct (PAN-38 round 4).
                    self.cq_no_response_streak = self.cq_no_response_streak.saturating_sub(1);
                    debug!(
                        "Autonomous self-CQ attempt {} failed downstream after a newer attempt \
                         already superseded it -- compensated streak by -1 (attempt did not \
                         switch, so offset needed no correction)",
                        attempt_id
                    );
                } else {
                    debug!(
                        "Autonomous self-CQ attempt {} failed downstream after a newer attempt \
                         already superseded it, but the streak was independently invalidated \
                         (directed reply or collision jitter) since -- streak NOT \
                         auto-corrected (no longer purely additive across that gap); state may \
                         be off by one attempt",
                        attempt_id
                    );
                }
                return;
            }
        }
        debug!(
            "Autonomous self-CQ attempt {} failed downstream but is more than one generation \
             stale -- state not corrected",
            attempt_id
        );
    }

    fn push_skip(&mut self, record: CqSkipRecord) {
        if self.skip_log.len() < MAX_SKIP_LOG_PER_SLOT {
            self.skip_log.push(record);
        }
    }

    /// Share the coordinator's TX-frequency-mode atomic so the smart-frequency
    /// allocator and collision-listen jitter respect the operator's Hold/Auto
    /// choice at runtime. Pass the same `Arc<AtomicU8>` the TUI toggle updates
    /// (encoded via [`pancetta_core::TxFreqMode::as_u8`]). If never called, the
    /// operator keeps its private `Hold` default (frequency is never moved
    /// autonomously).
    pub fn set_tx_freq_mode_source(&mut self, source: std::sync::Arc<std::sync::atomic::AtomicU8>) {
        self.tx_freq_mode = source;
    }

    /// Share the coordinator's TX-frequency-mode generation counter (PAN-39)
    /// — bumped once per Hold/Auto transition, alongside `tx_freq_mode`
    /// itself. Pass the same `Arc<AtomicU32>` `tui_relay.rs`'s command
    /// handlers increment on every `tx_freq_mode` store. If never called,
    /// the operator keeps a private counter that's never bumped, so the
    /// generation check is always a no-op — same "unwired → today's
    /// behavior" convention as `set_tx_freq_mode_source`.
    pub fn set_tx_freq_mode_generation_source(
        &mut self,
        source: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        self.tx_freq_mode_generation = source;
    }

    /// Share the coordinator's live parked-TX-offset atomic
    /// (`tx_offset_hold_hz`) so Hold-mode frequency allocation reflects the
    /// operator's actual parked offset (set via the TUI's `o` modal) instead
    /// of the static config value baked in at construction. Pass the same
    /// `Arc<AtomicU64>` the coordinator's `tx_offset_hold_hz()` accessor
    /// returns. If never called, Hold-mode uses `config.tx_offset_hz` only —
    /// today's behavior.
    pub fn set_tx_offset_hold_source(
        &mut self,
        hold_hz: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.tx_offset_hold_hz = Some(hold_hz);
    }

    /// Current TX-frequency mode (decoded from the shared atomic).
    /// PAN-38 round 3 (Codex): `SeqCst`, paired with `SeqCst` on the
    /// generation counter below and on `tui_relay.rs`'s writer side (which
    /// now bumps the generation BEFORE storing the new mode). Plain
    /// Acquire/Release on two independent atomics only synchronizes a
    /// specific acquire with the specific release it observes -- it does
    /// NOT guarantee that seeing this thread's mode-store also means
    /// seeing an EARLIER store to the unrelated generation counter. `SeqCst`
    /// gives a single total order across both atomics on both sides, which
    /// is what `decide_at`'s "mode read first, generation read second"
    /// sequence actually needs: if this load observes the NEW mode, the
    /// generation load right after it is then guaranteed to observe the
    /// writer's already-completed bump too, closing the window where a
    /// concurrent Hold->Auto transition was visible via the mode read but
    /// not yet via the generation read (in which case neither invalidation
    /// check would have fired).
    fn tx_freq_auto(&self) -> bool {
        pancetta_core::TxFreqMode::from_u8(
            self.tx_freq_mode.load(std::sync::atomic::Ordering::SeqCst),
        )
        .allows_auto_change()
    }

    /// `true` if `text` decodes as a genuine directed reply to us — used to
    /// reset the no-response CQ streak. Deliberately reuses the real FT8
    /// message parser (`self.exchange.parse_message`) and compound/hash-
    /// aware callsign matching (`callsigns_match`) instead of a hand-rolled
    /// text-shape heuristic: those correctly handle every valid rung shape
    /// (report, R-report, RR73/RRR/73, and a grid-less bare-call
    /// acknowledgment — `MessageType::CqResponse { grid: None, .. }`, the
    /// Type-4 compound-callsign encoder's grid-dropping form), compound
    /// callsigns, and i3=4 hash-rendered callsigns (`<K5ARH>`) — all things
    /// a plain 3-token-with-payload check would reject as malformed.
    ///
    /// Distinct from `active_qso_count` (which our own not-yet-timed-out
    /// self-CQ can also make nonzero — see `Self::cq_no_response_streak`).
    fn is_directed_response(&self, text: &str) -> bool {
        match self.exchange.parse_message(text) {
            // The first reply to a CQ: field order is "<addressee>
            // <replier> [grid]" — `calling_station` (the addressee, per
            // states.rs's own doc and exchange.rs's parser) is US when
            // someone is replying to our CQ. Checking `responding_station`
            // too would be checking "did WE reply to someone else's CQ,"
            // which a genuine RX decode should never show (Codex review,
            // PR #276 round 2 — an earlier version of this checked both
            // sides on a mistaken belief that hash-rendering could land in
            // either position).
            Ok(MessageType::CqResponse {
                calling_station, ..
            }) => callsigns_match(&calling_station, &self.our_callsign),
            Ok(MessageType::SignalReport { to_station, .. })
            | Ok(MessageType::ReportAck { to_station, .. })
            | Ok(MessageType::FinalConfirmation { to_station, .. })
            | Ok(MessageType::SeventyThree { to_station, .. })
            | Ok(MessageType::ContestExchange { to_station, .. }) => {
                callsigns_match(&to_station, &self.our_callsign)
            }
            // A CQ (not a reply), non-standard text, or a parse/validation
            // failure (malformed callsign, wrong token shape, etc.) — none
            // of these count as a genuine response.
            Ok(MessageType::Cq { .. }) | Ok(MessageType::NonStandard { .. }) | Err(_) => false,
        }
    }

    /// Phase-5 hardening #1: install the callsign-continuity FP filter
    /// so the TX decision path can reject CQs from callsigns absent
    /// from the trust set (ADIF + cqdx + seed + accepted-decode rolling
    /// window). Called once by the coordinator after constructing the
    /// filter. Passing `None` (or never calling this) leaves CQ
    /// responses unfiltered — the decode-side filter still runs.
    pub fn set_fp_filter(
        &mut self,
        filter: Option<std::sync::Arc<crate::callsign_continuity::CallsignContinuityFilter>>,
    ) {
        self.fp_filter = filter;
    }

    // -- external inputs ----------------------------------------------------

    /// Feed decoded messages from the most recent RX slot so the operator
    /// can score CQs and check for collisions.
    ///
    /// Uses the real wall clock for the DX-busy bookkeeping. The
    /// deterministic sim harness calls [`Self::feed_decoded_messages_at`]
    /// with the virtual slot `now` instead; this method just forwards to it
    /// with `Utc::now()`, so production behavior is unchanged.
    pub fn feed_decoded_messages(
        &mut self,
        messages: &[DecodedMessageInfo],
        evaluator: &dyn DxEvaluator,
    ) {
        self.feed_decoded_messages_at(messages, evaluator, Utc::now());
    }

    /// Feed decoded messages, stamping DX-busy bookkeeping at an explicit
    /// `now` (testable / sim-drivable). [`Self::feed_decoded_messages`]
    /// forwards here with `Utc::now()` so production behavior is identical.
    pub fn feed_decoded_messages_at(
        &mut self,
        messages: &[DecodedMessageInfo],
        evaluator: &dyn DxEvaluator,
        now: DateTime<Utc>,
    ) {
        // Auto-parity detection. FQ-F2: stamp each message's activity under
        // its OWN decoded slot parity (`m.slot_parity`) rather than the
        // wall-clock parity at feed time — a decode completed just before a
        // slot boundary must not be attributed to the next slot just because
        // this function happens to run after the boundary. Messages without
        // a tracked parity (test scaffolding / untracked decodes) fall back
        // to the wall-clock parity, preserving existing behavior for callers
        // that don't set `slot_parity`.
        let current_parity = SlotParity::current();
        let mut even_count: u32 = 0;
        let mut odd_count: u32 = 0;
        let mut untracked_count: u32 = 0;
        for m in messages {
            match m.slot_parity {
                Some(pancetta_core::slot::SlotParity::Even) => even_count += 1,
                Some(pancetta_core::slot::SlotParity::Odd) => odd_count += 1,
                None => untracked_count += 1,
            }
        }
        if even_count > 0 {
            self.slot_manager
                .record_slot_activity(SlotParity::Even, even_count);
        }
        if odd_count > 0 {
            self.slot_manager
                .record_slot_activity(SlotParity::Odd, odd_count);
        }
        if untracked_count > 0 {
            self.slot_manager
                .record_slot_activity(current_parity, untracked_count);
        }

        // Band-hopping activity tracking.
        self.band_strategy.record_activity(messages.len() as u32);

        // Update frequency allocator with observed activity.
        self.frequency_allocator.update_observed(messages);

        // Record decode history for smart frequency allocation. Same
        // per-message-parity fix as above (FQ-F2): use each message's own
        // `slot_parity` when present, falling back to the wall-clock-derived
        // slot only when it's `None`.
        let current_slot = if current_parity == SlotParity::Even {
            TimeSlot::First
        } else {
            TimeSlot::Second
        };
        let records: Vec<DecodeRecord> = messages
            .iter()
            .map(|m| DecodeRecord {
                frequency_hz: m.frequency_hz,
                time_slot: match m.slot_parity {
                    Some(pancetta_core::slot::SlotParity::Even) => TimeSlot::First,
                    Some(pancetta_core::slot::SlotParity::Odd) => TimeSlot::Second,
                    None => current_slot,
                },
            })
            .collect();
        self.decode_history.push_cycle(records);

        // DX-busy tracking: record any station seen in a non-CQ exchange
        // (report / RR73 / 73 not directed at us) so we can yield to a DX
        // that is mid-QSO with a third party. Prune entries older than the
        // configured busy window so the map stays bounded.
        let busy_now = now;
        let busy_window = ChronoDuration::seconds(self.config.dx_busy_window_secs as i64);
        let cutoff = busy_now - busy_window;
        self.recently_in_qso.retain(|_, t| *t > cutoff);
        for msg in messages {
            for call in third_party_exchange_callsigns(&msg.message_text, &self.our_callsign) {
                self.recently_in_qso.insert(call, busy_now);
            }
        }

        // No-response CQ-streak reset: a genuine reply directed at us (the
        // standard "<us> <them> <payload>" exchange format, not a CQ) means
        // someone answered — reset the streak regardless of whether that
        // reply goes on to become a tracked QSO. Deliberately NOT keyed off
        // `active_qso_count`: our own self-CQ opens a `CallingCq` QSO that is
        // itself "active" until it times out, so that signal would reset the
        // streak against our own unanswered call, not a real response.
        if messages
            .iter()
            .any(|m| self.is_directed_response(&m.message_text))
        {
            self.cq_no_response_streak = 0;
            // PAN-38 round 3/4: this reset happens OUTSIDE any decide_at
            // snapshot's own speculative-mutation sequence -- a pending
            // restore/bounded-compensation for an older attempt must not
            // clobber a newer, genuine directed-reply reset. Streak-only
            // (this reset never touches the offset).
            self.streak_generation = self.streak_generation.wrapping_add(1);
        }

        // Extract CQ candidates.
        self.pending_cqs.clear();
        for msg in messages {
            if is_cq_message(&msg.message_text) {
                if let Some(ref call) = msg.callsign {
                    // Don't respond to our own CQ.
                    if call.eq_ignore_ascii_case(&self.our_callsign) {
                        continue;
                    }

                    let grid = extract_grid_from_cq(&msg.message_text);
                    let snr = msg.snr.clamp(-128, 127) as i8;
                    let score = evaluator.evaluate_cq(call, grid.as_deref(), snr, msg.frequency_hz);

                    // DX watchlist (#197): remember PerBandDxccNew+/Atno CQs
                    // heard this cycle regardless of what decide_at() goes on
                    // to do with them — bridges the "heard while at TX
                    // capacity" and "lost this cycle's single pounce slot"
                    // gaps. Never triggers a transmission by itself; see
                    // pancetta_qso::watchlist module docs.
                    if let Some(tiered) =
                        evaluator.evaluate_cq_tiered(call, grid.as_deref(), snr, msg.frequency_hz)
                    {
                        if tiered.tier >= PriorityTier::PerBandDxccNew {
                            self.watchlist
                                .refresh(call, grid.as_deref(), tiered.tier, now);
                        }
                    }

                    self.pending_cqs.push(CqCandidate {
                        callsign: call.clone(),
                        grid,
                        snr,
                        frequency_hz: msg.frequency_hz,
                        dx_score: score,
                        slot_parity: msg.slot_parity,
                        message_text: msg.message_text.clone(),
                        confidence: msg.confidence,
                        time_offset_s: msg.time_offset_s,
                        decode_origin: msg.decode_origin,
                    });
                }
            }
        }
        self.watchlist.prune(now);

        // Sort: best score first.
        self.pending_cqs.sort_by(|a, b| {
            b.dx_score
                .partial_cmp(&a.dx_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Update the spectral snapshot from WaterfallData.
    /// Call this each decode cycle with the latest power data.
    pub fn update_spectral(&mut self, snapshot: SpectralSnapshot) {
        self.spectral_snapshot = Some(snapshot);
    }

    /// Update live spot frequencies from cqdx.io for frequency nudging.
    pub fn update_live_spots(&mut self, spots: &[(f64, f64)]) {
        self.live_spot_frequencies = spots.to_vec();
    }

    // -- per-callsign rate limit (I-1) ----------------------------------------

    /// Returns `true` if we initiated a response to `callsign` within the
    /// last [`RECENT_RESPONSE_WINDOW_SECS`] (60 s). Caller should skip the
    /// CQ. `now` is supplied explicitly so the sim harness can drive it
    /// from the virtual clock; production passes `Utc::now()`.
    pub fn is_recently_responded_to(&self, callsign: &str, now: DateTime<Utc>) -> bool {
        let window = ChronoDuration::seconds(RECENT_RESPONSE_WINDOW_SECS);
        self.recently_responded_to
            .get(callsign)
            .is_some_and(|t| now.signed_duration_since(*t) < window)
    }

    /// Returns `true` if `callsign` was seen working a third party (a
    /// non-CQ exchange not directed at us) within
    /// `config.dx_busy_window_secs`. The autonomous operator suppresses
    /// new auto-responses to such a station even if it briefly CQs again.
    /// `now` is supplied explicitly so the sim harness can drive it from
    /// the virtual clock; production passes `Utc::now()`.
    pub fn is_dx_busy(&self, callsign: &str, now: DateTime<Utc>) -> bool {
        let window = ChronoDuration::seconds(self.config.dx_busy_window_secs as i64);
        self.recently_in_qso
            .get(callsign)
            .is_some_and(|t| now.signed_duration_since(*t) < window)
    }

    /// Record that we just initiated a response to `callsign`. Also
    /// opportunistically prunes entries older than 5 × the window so the
    /// map doesn't grow unbounded. `now` is supplied explicitly for the
    /// sim harness; production passes `Utc::now()`.
    fn mark_responded_to(&mut self, callsign: &str, now: DateTime<Utc>) {
        let cutoff = now - ChronoDuration::seconds(RECENT_RESPONSE_WINDOW_SECS * 5);
        self.recently_responded_to.retain(|_, t| *t > cutoff);
        self.recently_responded_to.insert(callsign.to_string(), now);
    }

    // -- frequency allocation -------------------------------------------------

    /// CQ-mode live-spot rarity nudge, applied in place and then re-sorted
    /// by score descending.
    ///
    /// Boosts every candidate within 200 Hz of a live cqdx spot whose
    /// rarity exceeds 0.7 by `0.2 * rarity` (additive; multiple overlapping
    /// spots stack). Shared by [`Self::allocate_smart_frequency`] (the real
    /// CQ-frequency decision) and [`Self::placement_snapshot`] (the TX-
    /// placement instrument) so both agree on the ranked order — the
    /// single-scorer invariant. Callers gate the call on
    /// `dx_target_hz.is_none() && !live_spot_frequencies.is_empty()`
    /// (this nudge only applies to general CQ-mode ranking, not pouncing on
    /// a specific DX).
    fn apply_live_spot_rarity_boost(
        candidates: &mut [FrequencyCandidate],
        live_spot_frequencies: &[(f64, f64)],
    ) {
        for candidate in candidates.iter_mut() {
            for &(spot_freq, spot_rarity) in live_spot_frequencies {
                let distance = (candidate.offset_hz - spot_freq).abs();
                if distance < 200.0 && spot_rarity > 0.7 {
                    candidate.score += 0.2 * spot_rarity;
                }
            }
        }
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Get the best frequency for a new QSO using the smart allocator.
    /// Falls back to the legacy allocator if no spectral data is available.
    fn allocate_smart_frequency(
        &self,
        dx_target_hz: Option<f64>,
        target_parity: Option<pancetta_core::slot::SlotParity>,
        avoid_hz: Option<f64>,
    ) -> f64 {
        // Hold mode (default): pancetta does not choose the offset — every
        // autonomous transmission goes out on the operator's pinned offset.
        // Prefer the LIVE parked offset (set via the TUI's `o` modal) over
        // the static config value when one is actually parked (non-zero);
        // `0` is the shared "unset" sentinel, so an unparked atomic (or no
        // source wired at all) falls back to `config.tx_offset_hz` exactly
        // as before this fix (FQ-F6).
        if !self.tx_freq_auto() {
            if let Some(ref hold) = self.tx_offset_hold_hz {
                let parked_hz = hold.load(std::sync::atomic::Ordering::Relaxed);
                if parked_hz != 0 {
                    return parked_hz as f64;
                }
            }
            return self.config.tx_offset_hz;
        }

        let own_freqs: Vec<f64> = self
            .frequency_allocator
            .own_frequencies()
            .values()
            .copied()
            .collect();

        // FQ-F8: map the caller's known TX slot parity (for a pounce, the
        // opposite of the DX's own observed slot — same convention as the
        // `tx_parity = cq.slot_parity.map(|p| p.opposite())` this operator
        // already latches for the actual transmission, see `decide()`'s
        // pounce arm) into the `TimeSlot` `rank_candidates_with_parity`
        // scores against. `None` (e.g. a self-CQ, which can commit to
        // either slot) degrades to the slot-blind scoring `rank_candidates`
        // already provided.
        let target_slot = target_parity.map(|p| match p {
            pancetta_core::slot::SlotParity::Even => TimeSlot::First,
            pancetta_core::slot::SlotParity::Odd => TimeSlot::Second,
        });

        if let Some(ref spectral) = self.spectral_snapshot {
            let mut candidates = self.smart_allocator.rank_candidates_with_parity(
                spectral,
                &self.decode_history,
                &own_freqs,
                dx_target_hz,
                target_slot,
            );

            // When calling CQ, prefer frequencies near rare DX spots.
            if dx_target_hz.is_none() && !self.live_spot_frequencies.is_empty() {
                Self::apply_live_spot_rarity_boost(&mut candidates, &self.live_spot_frequencies);
            }

            // Codex review (PR #276, round 6): `avoid_hz` needs a HARD
            // exclusion, not the soft -50 penalty `own_frequencies` scoring
            // applies (score_candidate's comment "effectively eliminates
            // this candidate" is aspirational, not guaranteed — on a
            // crowded/noisy band the penalized offset can still outrank
            // every alternative). Filtering the ranked list directly, only
            // for this specific avoid_hz, keeps the shared own_frequencies
            // soft-penalty semantics unchanged for its original callers
            // (own-QSO separation).
            let min_separation_hz = self.smart_allocator.config().min_separation_hz;
            let excluded: Vec<_> = match avoid_hz {
                Some(avoid) => candidates
                    .iter()
                    .filter(|c| (c.offset_hz - avoid).abs() >= min_separation_hz)
                    .cloned()
                    .collect(),
                None => candidates.clone(),
            };

            if let Some(best) = excluded.first() {
                return best.offset_hz;
            }

            // Codex review (PR #277, round 3): if the hard exclusion above
            // filters out every ranked candidate (e.g. min_separation_hz
            // configured larger than the available spectral spread), there
            // is genuinely no frequency that honors the exclusion. Don't
            // fall through to the legacy `allocate_cq_frequency()` below
            // (avoid_hz-blind, could reselect exactly the abandoned
            // frequency) — and don't pick a "farthest available"
            // compromise either (round 2's fix), since that still openly
            // violates the configured separation the caller asked for.
            // Return `avoid` itself unchanged: the caller (the
            // no-response-streak switch path in `decide_at`) treats a
            // no-op result as "no valid relocation" and skips the switch
            // entirely, matching the existing "stale occupancy data" skip
            // path rather than committing to a frequency that isn't
            // actually excluded.
            if let Some(avoid) = avoid_hz {
                return avoid;
            }
        }

        // Fallback: legacy allocator
        self.frequency_allocator.allocate_cq_frequency()
    }

    /// Rank the current band openness for the TX-placement instrument.
    ///
    /// Returns `None` until the first spectral snapshot arrives. This is a
    /// pure read — it does NOT allocate or mutate — over the SAME
    /// `smart_allocator` / `decode_history` / `spectral_snapshot` /
    /// `frequency_allocator` fields [`Self::allocate_smart_frequency`] uses
    /// to make real TX-frequency decisions (single-scorer invariant): the
    /// TUI instrument never re-derives scores from a separate computation.
    pub fn placement_snapshot(&self, top_n: usize) -> Option<PlacementSnapshot> {
        let spectral = self.spectral_snapshot.as_ref()?;
        let own: Vec<f64> = self
            .frequency_allocator
            .own_frequencies()
            .values()
            .copied()
            .collect();
        let mut cands =
            self.smart_allocator
                .rank_candidates(spectral, &self.decode_history, &own, None);

        // Same CQ-mode live-spot rarity nudge `allocate_smart_frequency`
        // applies (dx_target_hz is always None here, so the gate collapses
        // to just the live-spots check) — single-scorer invariant: this
        // instrument must never diverge from the real decision's ranking.
        if !self.live_spot_frequencies.is_empty() {
            Self::apply_live_spot_rarity_boost(&mut cands, &self.live_spot_frequencies);
        }

        cands.truncate(top_n);

        let (min_f, max_f) = self.smart_allocator.config().range;
        let bin_hz = self.smart_allocator.config().step_hz;
        let bins = ((max_f - min_f) / bin_hz).ceil() as usize;
        let openness = (0..bins)
            .map(|i| {
                let f = min_f + i as f64 * bin_hz;
                let cf = self
                    .decode_history
                    .activity_near_in_slot(f, 50.0, TimeSlot::First)
                    == 0;
                let cs = self
                    .decode_history
                    .activity_near_in_slot(f, 50.0, TimeSlot::Second)
                    == 0;
                match (cf, cs) {
                    (true, true) => 3u8,
                    (true, false) => 2,
                    (false, true) => 1,
                    (false, false) => 0,
                }
            })
            .collect();

        Some(PlacementSnapshot {
            slices: cands,
            openness,
            bin_hz,
            range: (min_f, max_f),
        })
    }

    /// Test-only mutable accessor to `decode_history`, so tests can seed
    /// occupancy without going through `feed_decoded_messages`.
    #[cfg(test)]
    pub(crate) fn decode_history_mut_for_test(&mut self) -> &mut DecodeHistory {
        &mut self.decode_history
    }

    /// Tell the operator how many QSOs the auto-sequencer is currently managing.
    pub fn set_active_qso_count(&mut self, count: u32) {
        self.active_qso_count = count;
    }

    /// Feed a message the auto-sequencer wants to send this cycle.
    /// For backward compatibility, replaces any pending messages.
    pub fn set_pending_sequencer_message(&mut self, message_text: String, qso_id: Option<String>) {
        self.pending_sequencer_messages.clear();
        self.pending_sequencer_messages
            .push((message_text, self.config.tx_offset_hz, qso_id));
    }

    /// Add a sequencer message for a specific QSO at a specific frequency.
    /// Used for multi-QSO operation where each QSO has its own frequency.
    pub fn add_pending_sequencer_message(
        &mut self,
        message_text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
    ) {
        self.pending_sequencer_messages
            .push((message_text, frequency_offset, qso_id));
    }

    /// Clear all pending sequencer messages (called after decide()).
    pub fn clear_pending_sequencer_messages(&mut self) {
        self.pending_sequencer_messages.clear();
    }

    /// Access the frequency allocator for external QSO frequency management.
    pub fn frequency_allocator(&self) -> &FrequencyAllocator {
        &self.frequency_allocator
    }

    /// Mutable access to the frequency allocator.
    pub fn frequency_allocator_mut(&mut self) -> &mut FrequencyAllocator {
        &mut self.frequency_allocator
    }

    /// Currently-watchlisted callsigns, for TUI/status surfacing. Read-only —
    /// mirrors `placement_snapshot`'s "instrument, not a decision" pattern.
    pub fn watchlist_callsigns(&self) -> Vec<String> {
        self.watchlist.callsigns()
    }

    pub fn pause(&mut self) {
        self.paused = true;
        self.state = OperatingState::Paused;
    }

    pub fn resume(&mut self) {
        self.paused = false;
        self.state = OperatingState::Hunting;
    }

    pub fn toggle_pause(&mut self) {
        if self.paused {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn state(&self) -> &OperatingState {
        &self.state
    }

    pub fn config(&self) -> &AutonomousConfig {
        &self.config
    }

    pub fn slot_parity(&self) -> Option<SlotParity> {
        self.slot_manager.our_slot
    }

    pub fn tx_offset_hz(&self) -> f64 {
        self.config.tx_offset_hz
    }

    /// Shift our TX offset, e.g. after a collision.
    pub fn set_tx_offset(&mut self, offset: f64) {
        self.config.tx_offset_hz = offset;
        self.collision_detector.our_tx_offset_hz = offset;
    }

    // -- the per-cycle decision engine --------------------------------------

    /// Run one cycle of the decision engine. Returns zero or more actions.
    pub fn decide(&mut self) -> Vec<OperatorAction> {
        self.decide_at(Utc::now().timestamp())
    }

    /// Run one cycle at a specific unix timestamp (for testing).
    /// Invalidate `current_cq_offset_hz` if a Hold/Auto transition has been
    /// observed (directly, or via the generation counter for one squeezed
    /// entirely between polls). Called at the top of `decide_at` AND again
    /// immediately before the routine (non-switching) self-CQ path actually
    /// CONSUMES `current_cq_offset_hz`, several hundred lines later in the
    /// same call.
    ///
    /// PAN-38 round 4 (Codex): a Hold->Auto round trip can complete AFTER
    /// the top-of-`decide_at` check but BEFORE the self-CQ branch reads
    /// `current_cq_offset_hz` -- both the mode store and its generation
    /// bump are made by a writer thread (`tui_relay.rs`) that holds no lock
    /// this operator's own mutation is serialized under, so nothing
    /// prevents them from landing in the (admittedly narrow, since
    /// `decide_at` never yields) window between the two checks. Revalidate
    /// immediately before consumption rather than trusting a check from
    /// earlier in the same call.
    fn refresh_tx_freq_offset_invalidation(&mut self) {
        // Codex review (PR #276, round 6): invalidate any stale Auto-mode
        // sticky offset as soon as Hold mode is observed, every cycle — not
        // only when a Hold-mode self-CQ happens to run. Waiting for a
        // Hold-mode CQ meant an operator who toggled Auto -> Hold -> Auto
        // between CQ opportunities (or simply never got idle enough to CQ
        // while in Hold) would resume Auto on a stale pre-Hold value instead
        // of re-ranking fresh.
        if !self.tx_freq_auto() {
            self.current_cq_offset_hz = None;
            // PAN-38 round 3/4: bump the offset-invalidation generation so a
            // pending restore of an OLDER snapshot (taken before this Hold
            // observation) can no longer resurrect the offset just cleared.
            self.offset_generation = self.offset_generation.wrapping_add(1);
        }

        // PAN-39: the check above only catches Hold observed AT THIS POLL.
        // An Auto -> Hold -> Auto round trip that completes entirely between
        // two `decide_at` cycles is invisible to it — by the time this cycle
        // polls, the atomic already reads Auto again. The generation counter
        // catches that: any change since the last cycle we observed means at
        // least one transition happened in between, so the pre-transition
        // sticky offset can no longer be trusted, independent of what the
        // mode reads as right now.
        // PAN-39 round 1 (Codex): must pair with the writer's `Release`
        // store (`tui_relay.rs`'s `fetch_add(1, Ordering::Release)`) via
        // `Acquire`, not `Relaxed` -- on a weakly-ordered target a relaxed
        // load can observe a newer `tx_freq_mode` value before the
        // corresponding generation increment becomes visible to this
        // thread, letting one more self-CQ reuse the stale offset before
        // the next poll notices the generation change.
        //
        // PAN-38 round 3 (Codex): `Acquire` alone still isn't enough --
        // this load and `tx_freq_auto()`'s mode load right above are two
        // INDEPENDENT atomics, and a plain acquire only synchronizes with
        // the specific release it happens to observe. `tui_relay.rs` now
        // bumps this generation counter BEFORE storing the new mode value
        // (SeqCst on both sides), and this load is upgraded to `SeqCst` to
        // match -- see `tx_freq_auto()`'s doc comment for the full
        // reasoning on why the combination closes the race Acquire/Release
        // alone left open.
        let current_tx_freq_mode_generation = self
            .tx_freq_mode_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        if current_tx_freq_mode_generation != self.last_seen_tx_freq_mode_generation {
            self.current_cq_offset_hz = None;
            self.last_seen_tx_freq_mode_generation = current_tx_freq_mode_generation;
            // PAN-38 round 3/4: same reasoning as the direct Hold-mode check
            // above -- an invisible-between-polls transition invalidates a
            // pending restore's snapshot just as much as an observed one.
            self.offset_generation = self.offset_generation.wrapping_add(1);
        }
    }

    pub fn decide_at(&mut self, unix_secs: i64) -> Vec<OperatorAction> {
        self.skip_log.clear();
        let mut actions = Vec::new();

        self.refresh_tx_freq_offset_invalidation();

        if self.paused {
            actions.push(self.status_action());
            return actions;
        }

        // Step 0: band hopping
        if let Some(new_freq) = self.band_strategy.should_hop() {
            actions.push(OperatorAction::ChangeBand {
                dial_frequency: new_freq,
            });
            // Codex review (PR #276): decode_history/spectral_snapshot/
            // current_cq_offset_hz are all scoped to the OLD band's audio-
            // offset space — a dial change makes them meaningless (and, for
            // the no-response-streak freshness check below, misleadingly
            // "fresh") for ranking on the new band. Clear them so both the
            // ordinary allocator and the streak-switch logic wait for real
            // data from the new band before trusting any ranking.
            self.decode_history = DecodeHistory::new(self.config.frequency.decode_history_cycles);
            self.spectral_snapshot = None;
            self.current_cq_offset_hz = None;
            // PAN-38 round 3/4: a band hop invalidates the old band's sticky
            // offset exactly like a mode transition does -- a pending
            // restore of a pre-hop snapshot must not resurrect it.
            self.offset_generation = self.offset_generation.wrapping_add(1);
        }

        // Step 1: slot manager
        let decision = self.slot_manager.should_transmit_at(unix_secs);

        match decision {
            SlotDecision::NotOurSlot => {
                // Not our slot. Just listen.
                actions.push(OperatorAction::Listen);
            }

            SlotDecision::Listen => {
                // Our slot but we should listen for collisions.
                self.state = OperatingState::ListeningForCollisions;
                actions.push(OperatorAction::CollisionListen);
            }

            SlotDecision::Transmit => {
                let mut tx_count = 0u32;

                // Step 2: emit all pending sequencer messages (active QSOs).
                if !self.pending_sequencer_messages.is_empty() {
                    let messages: Vec<_> = self.pending_sequencer_messages.drain(..).collect();
                    for (msg, freq, qso_id) in messages {
                        actions.push(OperatorAction::Transmit {
                            message_text: msg,
                            frequency_offset: freq,
                            qso_id,
                            // No parity here by design: this path is only fed via
                            // {set,add}_pending_sequencer_message, which production
                            // never calls — live mid-QSO TX flows through
                            // QsoManager::send_message → QsoEvent::MessageToSend,
                            // which carries the tx_parity latched at QSO start.
                            // The TX scheduler falls back to config self-parity.
                            tx_parity: None,
                        });
                        tx_count += 1;
                    }
                    self.state = OperatingState::InQso {
                        qso_count: self.active_qso_count,
                    };
                    self.idle_cycles = 0;
                }

                // Step 3: if we have capacity, try to respond to a CQ or call CQ.
                // active_qso_count already includes QSOs with pending sequencer messages,
                // so we don't add tx_count (that would double-count).
                let total_active = self.active_qso_count.max(tx_count);
                let can_add_new = total_active < self.config.max_concurrent_qsos;

                if !can_add_new && !self.pending_cqs.is_empty() {
                    self.push_skip(CqSkipRecord {
                        callsign: None,
                        reason: SkipReason::AtCapacity {
                            active: total_active,
                            cap: self.config.max_concurrent_qsos,
                        },
                    });
                }

                if can_add_new {
                    // Choose threshold: first QSO uses min_dx_score,
                    // additional QSOs use the higher min_multi_slot_score.
                    let threshold = if total_active == 0 {
                        self.config.min_dx_score
                    } else {
                        self.config.min_multi_slot_score
                    };

                    // Time-dependent gates (recently-responded, DX-busy)
                    // evaluate against the slot's virtual `now`, derived
                    // from the `unix_secs` this cycle runs at, so the sim
                    // harness drives them deterministically. Production
                    // calls `decide()` which passes `Utc::now().timestamp()`,
                    // so the derived `now` equals real now to the second.
                    let now =
                        DateTime::<Utc>::from_timestamp(unix_secs, 0).unwrap_or_else(Utc::now);
                    // Phase-5 hardening #1: TX-side FP gate. If a
                    // callsign-continuity filter is installed, reject
                    // any CQ whose sender doesn't appear in the trust
                    // set (ADIF + cqdx + seed + rolling window).
                    // Mirrors the decode-side filter; catches CQs from
                    // OSD fabrications that slipped through (e.g. the
                    // first one before the rolling window populated).
                    let fp = self.fp_filter.clone();
                    let slot_skips = std::cell::RefCell::new(Vec::new());
                    let best_cq = self
                        .pending_cqs
                        .iter()
                        .filter(|cq| cq.dx_score >= threshold)
                        .filter(|cq| {
                            let recent = self.is_recently_responded_to(&cq.callsign, now);
                            if recent {
                                slot_skips.borrow_mut().push(CqSkipRecord {
                                    callsign: Some(cq.callsign.clone()),
                                    reason: SkipReason::RecentlyResponded,
                                });
                            }
                            !recent
                        })
                        // DX-busy gate: do not start an auto-response to a
                        // station that was just working a third party, even
                        // if it CQs again mid-sequence.
                        .filter(|cq| {
                            let busy = self.is_dx_busy(&cq.callsign, now);
                            if busy {
                                debug!(
                                    target: "qso.security",
                                    "suppressing CQ response: DX recently working a third party (callsign={}, window={}s)",
                                    cq.callsign, self.config.dx_busy_window_secs
                                );
                                slot_skips.borrow_mut().push(CqSkipRecord {
                                    callsign: Some(cq.callsign.clone()),
                                    reason: SkipReason::DxBusy {
                                        window_secs: self.config.dx_busy_window_secs,
                                    },
                                });
                            }
                            !busy
                        })
                        .filter(|cq| match fp.as_ref() {
                            None => true,
                            Some(f) => {
                                let ok = f.would_accept_callsign(&cq.callsign);
                                if !ok {
                                    debug!(
                                        target: "qso.security",
                                        "rejecting CQ response: callsign continuity (callsign={}, score={:.2})",
                                        cq.callsign, cq.dx_score
                                    );
                                    slot_skips.borrow_mut().push(CqSkipRecord {
                                        callsign: Some(cq.callsign.clone()),
                                        reason: SkipReason::CallsignContinuity {
                                            dx_score: cq.dx_score,
                                        },
                                    });
                                }
                                ok
                            }
                        })
                        // hb-103 (Batch 32): content-score gate.
                        //
                        // CQs are single-callsign messages, so the
                        // in_trust_set_both bonus (+2) cannot fire — the
                        // achievable score range for legitimate CQs sits
                        // below SHIP_PRECISE (+2.977, calibrated on the full
                        // hard-200 message mix that includes 2-callsign
                        // exchanges). For autonomous CQ gating we use
                        // **SHIP_CONSERVATIVE (+0.35)** which preserves
                        // 100% recall on the corpus while eliminating 34% of
                        // FPs (Diagnostic T). Reply / report messages with
                        // two trusted callsigns would warrant SHIP_PRECISE.
                        //
                        // When the FP filter is installed AND the decode
                        // carries confidence + time_offset_s, compute the
                        // fused score and require it to clear the threshold.
                        // Decodes missing confidence/dt (test fixtures,
                        // pre-hb-103 paths) pass through.
                        .filter(|cq| match (fp.as_ref(), cq.confidence, cq.time_offset_s) {
                            (Some(f), Some(conf), Some(dt)) => {
                                use crate::content_score::{content_score_v3_from_features, ContentFeatures, MessageContentScore};
                                // hb-103 v3 gate (Batch 81): v3 at the
                                // unchanged SHIP_CONSERVATIVE threshold is
                                // byte-identical to the old v1 gate for
                                // primary-pass decodes (origin 0 → lateness
                                // term 0, FDR telemetry None → v2 ≡ v1) and
                                // strictly tightens recovery-pass decodes
                                // (-origin/6). Measured strictly dominant-
                                // or-equal to v1 at every τ on hard_200 +
                                // raw_530 CQ populations (Batch 81 note).
                                // The threshold itself is a separate
                                // operator-posture lever: 0.90 would add
                                // +9-12pp FP rejection at 100% measured
                                // recall but cuts the recall margin from
                                // 0.72 to 0.17 (v1 min-TP score = 1.07).
                                let score = content_score_v3_from_features(
                                    ContentFeatures {
                                        text: &cq.message_text,
                                        confidence: conf,
                                        snr_db: cq.snr as f32,
                                        time_offset: dt,
                                        // Batch 64: FDR ConfidenceFeatures
                                        // still not plumbed to this path;
                                        // v3 reduces to v1 + lateness term.
                                        bp_iterations_used: None,
                                        osd_depth_used: None,
                                        nharderrs: None,
                                        min_llr_magnitude: None,
                                        // hb-247 (Batch 81): deterministic
                                        // decode-origin ordinal / 6.
                                        lateness_frac: cq
                                            .decode_origin
                                            .map(|o| f64::from(o) / 6.0),
                                    },
                                    f,
                                );
                                let pass = score >= MessageContentScore::SHIP_CONSERVATIVE;
                                if !pass {
                                    debug!(
                                        target: "qso.security",
                                        "rejecting CQ response: hb-103 v3 content score (callsign={}, dx_score={:.2}, content_score={:.3}, threshold={:.3}, decode_origin={:?})",
                                        cq.callsign,
                                        cq.dx_score,
                                        score,
                                        MessageContentScore::SHIP_CONSERVATIVE,
                                        cq.decode_origin,
                                    );
                                    slot_skips.borrow_mut().push(CqSkipRecord {
                                        callsign: Some(cq.callsign.clone()),
                                        reason: SkipReason::ContentScore {
                                            score,
                                            threshold: MessageContentScore::SHIP_CONSERVATIVE,
                                        },
                                    });
                                }
                                pass
                            }
                            _ => true,
                        })
                        .find(|cq| {
                            let clear = self.frequency_allocator.is_clear_of_own(cq.frequency_hz);
                            if !clear {
                                slot_skips.borrow_mut().push(CqSkipRecord {
                                    callsign: Some(cq.callsign.clone()),
                                    reason: SkipReason::FrequencyClash,
                                });
                            }
                            clear
                        })
                        .cloned();

                    for record in slot_skips.into_inner() {
                        self.push_skip(record);
                    }

                    if let Some(cq) = best_cq {
                        if tx_count == 0 && self.active_qso_count == 0 {
                            self.state = OperatingState::Hunting;
                        }
                        self.idle_cycles = 0;

                        // Use smart allocator to find best TX frequency near the DX station.
                        // FQ-F8: our TX slot is the opposite of the DX's own observed
                        // slot — the SAME expression latched as `tx_parity` for the actual
                        // transmission below, so scoring and the real transmit decision can
                        // never disagree about which slot we're targeting.
                        let tx_freq = self.allocate_smart_frequency(
                            Some(cq.frequency_hz),
                            cq.slot_parity.map(|p| p.opposite()),
                            None,
                        );

                        let grid_part = self
                            .our_grid
                            .as_deref()
                            .map(|g| format!(" {}", g))
                            .unwrap_or_default();
                        let message_text =
                            format!("{} {}{}", cq.callsign, self.our_callsign, grid_part)
                                .trim()
                                .to_string();

                        debug!(
                            "Responding to CQ from {} (score={:.2}, snr={}) at {:.0} Hz (TX at {:.0} Hz)",
                            cq.callsign, cq.dx_score, cq.snr, cq.frequency_hz, tx_freq
                        );

                        actions.push(OperatorAction::Transmit {
                            message_text,
                            frequency_offset: tx_freq,
                            qso_id: None,
                            // We heard the CQ on cq.slot_parity; we respond on the opposite slot.
                            tx_parity: cq.slot_parity.map(|p| p.opposite()),
                        });
                        self.mark_responded_to(&cq.callsign, now);
                        tx_count += 1;
                    } else if tx_count == 0 && self.active_qso_count == 0 {
                        // Step 4: no CQs worth answering and no active QSOs — CQ ourselves?
                        self.idle_cycles += 1;

                        if self.idle_cycles >= self.config.cq_after_idle_cycles {
                            self.state = OperatingState::CallingCq;
                            self.idle_cycles = 0;

                            // (Codex review round 5's Hold-mode invalidation
                            // used to live here; round 6 moved it to the top
                            // of `decide_at` so it runs every cycle, not only
                            // when a Hold-mode CQ happens to fire — so by
                            // this point `current_cq_offset_hz` is already
                            // correctly `None` whenever we're in Hold mode.)
                            //
                            // Codex review (PR #276, round 4): captured HERE,
                            // not by the coordinator before calling decide()
                            // — this is after Step 0's band-hop/mode-driven
                            // invalidations already ran this cycle, but
                            // before this block's own speculative self-CQ
                            // mutations. A pre-decide() snapshot would
                            // "restore" a value Step 0 had just correctly
                            // invalidated (e.g. `current_cq_offset_hz`
                            // cleared on a same-cycle band hop) if this
                            // cycle's CQ then got suppressed downstream.
                            self.cq_attempt_counter += 1;
                            // PAN-38 round 1: shift the still-unresolved
                            // previous snapshot (if any) down before
                            // overwriting `last_cq_snapshot`, so a failure
                            // report for THAT attempt can still be found by
                            // `restore_cq_state_for_attempt` one generation
                            // later instead of being silently dropped.
                            self.previous_cq_snapshot = self.last_cq_snapshot.take();
                            self.last_cq_snapshot = Some(CqStateSnapshot {
                                streak: self.cq_no_response_streak,
                                current_cq_offset_hz: self.current_cq_offset_hz,
                                tx_offset_hz: self.config.tx_offset_hz,
                                attempt_id: self.cq_attempt_counter,
                                did_switch: false,
                                offset_generation: self.offset_generation,
                                streak_generation: self.streak_generation,
                            });

                            // PAN-38 round 5 (Codex): refresh HERE, before
                            // `should_switch` is decided, not only later
                            // right before `current_cq_offset_hz` is
                            // consumed (round 4) -- the round-4 refresh
                            // alone left `should_switch` computed from a
                            // stale `tx_freq_auto()` read: an Auto->Hold
                            // transition landing between this decision and
                            // that later refresh would correctly clear the
                            // sticky offset, but the STALE `should_switch
                            // == true` would still enter the switch branch
                            // below and dispatch an autonomously-selected
                            // frequency despite Hold mode. One refresh
                            // covers both this decision and the later
                            // consumption -- see
                            // `refresh_tx_freq_offset_invalidation`'s own
                            // doc comment.
                            self.refresh_tx_freq_offset_invalidation();

                            let should_switch = self.tx_freq_auto()
                                && self.cq_no_response_streak
                                    >= self.config.cq_no_response_switch_after;

                            if should_switch {
                                let history_fresh = self.spectral_snapshot.is_some()
                                    && self.decode_history.cycles_recorded()
                                        >= self.config.frequency.decode_history_cycles;

                                if !history_fresh {
                                    // Not enough fresh occupancy data to pick a good
                                    // alternative — skip this window and listen instead
                                    // of guessing blind. Streak stays put; retried next
                                    // window once history fills in from ordinary RX.
                                    self.state = OperatingState::Hunting;
                                    actions.push(OperatorAction::Listen);
                                    actions.push(self.status_action());
                                    return actions;
                                }
                            }

                            // Codex review (PR #276): the offset to avoid when
                            // switching is the one we ACTUALLY last transmitted a
                            // self-CQ on — `current_cq_offset_hz` — not
                            // `config.tx_offset_hz`, which the routine (non-
                            // switching) path below does not otherwise keep in
                            // sync (e.g. the live-spot rarity boost can pick a
                            // frequency other than the config seed).
                            //
                            // FQ-F8: a self-CQ can land on either slot parity, so
                            // there's no single known target at scoring time —
                            // degrades to the slot-blind scoring path.
                            //
                            // Auto mode is sticky between switches: a routine CQ
                            // reuses `current_cq_offset_hz` directly rather than
                            // re-ranking, so a threshold-driven switch's new
                            // offset remains the baseline for the next streak
                            // instead of the allocator immediately re-picking the
                            // frequency we just switched away from (nothing in
                            // the ranking inputs distinguishes "avoided because we
                            // chose to leave it" from "still the best spot").
                            //
                            // PAN-38 round 4 (Codex) originally revalidated
                            // AGAIN here, right before `current_cq_offset_hz`
                            // is consumed below. Round 5 (Codex) found that
                            // refreshing the offset in a SECOND, separate call
                            // without also recomputing `should_switch` just
                            // moved the inconsistency window rather than
                            // closing it: a mode flip landing between the two
                            // calls would correctly re-clear the offset here
                            // but leave `should_switch` (computed from the
                            // FIRST, now-stale read, above) pointed at the old
                            // decision -- still entering the switch branch
                            // despite Hold mode. A single refresh, immediately
                            // followed by deriving BOTH `should_switch` and
                            // the offset consumption from that one consistent
                            // read (moved up before `should_switch`'s
                            // computation), is what actually closes this --
                            // repeating the refresh without also re-deriving
                            // every decision that depends on it doesn't
                            // converge.
                            let cq_freq = if should_switch {
                                let avoid = self
                                    .current_cq_offset_hz
                                    .unwrap_or(self.config.tx_offset_hz);
                                let new_offset =
                                    self.allocate_smart_frequency(None, None, Some(avoid));

                                // Codex review (PR #277, round 3): a no-op
                                // result (new_offset == avoid) means
                                // allocate_smart_frequency found no
                                // candidate honoring the hard exclusion —
                                // committing to it anyway would transmit
                                // right back on the abandoned frequency
                                // while claiming a successful switch. Treat
                                // this exactly like the "stale occupancy
                                // data" case above: skip the window and
                                // listen instead, preserving the streak so
                                // the switch is retried once conditions
                                // improve.
                                if (new_offset - avoid).abs() < f64::EPSILON {
                                    self.state = OperatingState::Hunting;
                                    actions.push(OperatorAction::Listen);
                                    actions.push(self.status_action());
                                    return actions;
                                }

                                self.cq_no_response_streak = 0;
                                // PAN-38 round 1: mark this attempt's own
                                // snapshot as switch-performing, so a later
                                // bounded compensating rollback (if this
                                // attempt fails and is by then stale) knows
                                // NOT to attempt a simple streak decrement --
                                // see `restore_cq_state_for_attempt`.
                                if let Some(snapshot) = self.last_cq_snapshot.as_mut() {
                                    snapshot.did_switch = true;
                                }
                                actions.push(OperatorAction::FrequencyShift {
                                    new_offset_hz: new_offset,
                                });
                                new_offset
                            } else if self.tx_freq_auto() {
                                match self.current_cq_offset_hz {
                                    Some(freq) => freq,
                                    None => self.allocate_smart_frequency(None, None, None),
                                }
                            } else {
                                // Codex review (PR #276, round 2): Hold mode
                                // never uses a sticky offset — the invalidation
                                // (so a later Auto -> Hold -> Auto round trip
                                // re-ranks fresh instead of resuming a stale
                                // pre-Hold value) now happens above, BEFORE
                                // the snapshot is taken (round 5), not here.
                                self.allocate_smart_frequency(None, None, None)
                            };

                            if self.tx_freq_auto() {
                                self.current_cq_offset_hz = Some(cq_freq);
                                self.set_tx_offset(cq_freq);
                            }
                            self.cq_no_response_streak =
                                self.cq_no_response_streak.saturating_add(1);

                            let cq_text = if self.config.cq_direction.is_empty() {
                                format!(
                                    "CQ {} {}",
                                    self.our_callsign,
                                    self.our_grid.as_deref().unwrap_or("")
                                )
                            } else {
                                format!(
                                    "CQ {} {} {}",
                                    self.config.cq_direction,
                                    self.our_callsign,
                                    self.our_grid.as_deref().unwrap_or("")
                                )
                            }
                            .trim()
                            .to_string();

                            actions.push(OperatorAction::Transmit {
                                message_text: cq_text,
                                frequency_offset: cq_freq,
                                qso_id: None,
                                // Calling our own CQ — no DX context, scheduler uses config self-parity.
                                tx_parity: None,
                            });
                        } else {
                            self.state = OperatingState::Hunting;
                            actions.push(OperatorAction::Listen);
                        }
                    }
                }

                // If we emitted sequencer messages but nothing else, no extra Listen needed.
                if tx_count == 0
                    && actions.iter().all(|a| {
                        !matches!(a, OperatorAction::Listen | OperatorAction::Transmit { .. })
                    })
                {
                    actions.push(OperatorAction::Listen);
                }
            }
        }

        actions.push(self.status_action());
        actions
    }

    /// Handle the result of a collision-listen slot.
    pub fn process_collision_listen(
        &mut self,
        decoded: &[DecodedMessageInfo],
    ) -> Vec<OperatorAction> {
        let result = self.collision_detector.check_for_collision(decoded);
        let mut actions = Vec::new();

        if result.detected {
            warn!("Collision detected with: {:?}", result.interfering_calls);
            self.slot_manager
                .listen_policy
                .record_collision(&self.config.listen_cycle);

            // Hold our TX offset while a QSO is in progress (operator request).
            // The collision-listen jitter only seeds the *next* CQ/opening's
            // offset anyway (an active QSO transmits on its own latched
            // `QsoMetadata.frequency`, not this global offset), so jittering
            // mid-QSO would surprise the operator without helping the live QSO.
            // The QSO engine's own stuck-DX detector handles a collision on a
            // live QSO's held offset. When idle, jitter as before so the next
            // CQ avoids the interferer — and only ever from a collision-LISTEN
            // slot, i.e. a slot we listened on without transmitting (exactly the
            // "new information" precondition the operator described).
            //
            // ALSO gated by TX-freq mode: in the default Hold mode pancetta
            // never moves the offset on its own, so the jitter is suppressed
            // entirely (only Auto mode lets it shift off the interferer).
            if self.tx_freq_auto() && self.active_qso_count == 0 {
                // Pick a new offset with random jitter.
                let prev_offset = self
                    .current_cq_offset_hz
                    .unwrap_or(self.config.tx_offset_hz);
                let jitter = simple_jitter();
                let new_offset = (self.config.tx_offset_hz + jitter).clamp(200.0, 2800.0);
                self.set_tx_offset(new_offset);
                // Codex review (PR #276, round 2): this comment block already
                // documents the jitter as seeding the *next* CQ's offset —
                // without also updating the no-response-streak's sticky
                // baseline, the next self-CQ would read the stale
                // pre-jitter `current_cq_offset_hz` instead and silently
                // undo this jitter.
                self.current_cq_offset_hz = Some(new_offset);
                // Codex review (PR #277, round 1): `simple_jitter()` can
                // return 0, or a boundary clamp can snap `new_offset` back
                // to `prev_offset` — in either case no real frequency change
                // happened, so resetting the streak or reporting a
                // FrequencyShift here would be a no-op disguised as
                // collision avoidance, silently postponing the real
                // no-response-driven switch indefinitely.
                if (new_offset - prev_offset).abs() > f64::EPSILON {
                    // PAN-38 round 3/4: a genuine (non-no-op) jitter changes
                    // `current_cq_offset_hz` outside any decide_at snapshot's
                    // own speculative-mutation sequence -- bump so a pending
                    // restore of an older snapshot can't resurrect the
                    // pre-jitter offset. Streak generation is bumped
                    // separately below, only when the streak is actually
                    // reset too.
                    self.offset_generation = self.offset_generation.wrapping_add(1);
                    // Codex review (PR #277, round 3): a nonzero jitter
                    // isn't necessarily a nonzero jitter AWAY from the
                    // interferer — a small draw (e.g. ±1 Hz) can leave
                    // `new_offset` still within the collision detector's
                    // tolerance of the same decoded station. Only reset the
                    // no-response streak once we've actually escaped every
                    // interferer that triggered this collision check;
                    // otherwise the next CQ would still be colliding while
                    // the threshold-driven smart switch has been postponed
                    // for another full streak.
                    let still_colliding = decoded.iter().any(|msg| {
                        (msg.frequency_hz - new_offset).abs()
                            <= self.collision_detector.tolerance_hz
                    });

                    if !still_colliding {
                        // Codex review (PR #276, round 6): also reset the
                        // no-response streak. Without this, an idle Auto operator
                        // that already reached the switch threshold — collision
                        // jitter fires first and moves the offset — would still
                        // have a streak sitting at/above the threshold, so the very
                        // next self-CQ immediately re-enters the switch branch and
                        // moves away again without ever trying the frequency
                        // collision avoidance just picked.
                        self.cq_no_response_streak = 0;
                        // PAN-38 round 4: this streak reset happens OUTSIDE
                        // any decide_at snapshot's own speculative-mutation
                        // sequence -- bump the streak generation too, same
                        // reasoning as the directed-reply reset.
                        self.streak_generation = self.streak_generation.wrapping_add(1);
                    }

                    actions.push(OperatorAction::FrequencyShift {
                        new_offset_hz: new_offset,
                    });
                }
            }
        } else {
            self.slot_manager
                .listen_policy
                .record_clean_listen(&self.config.listen_cycle);
        }

        actions
    }

    fn status_action(&self) -> OperatorAction {
        OperatorAction::StatusUpdate(AutonomousStatusData {
            enabled: self.config.enabled && !self.paused,
            state: self.state.to_string(),
            slot_parity: self.slot_manager.our_slot.map(|p| format!("{:?}", p)),
            listen_counter: format!(
                "{}/{}",
                self.slot_manager.listen_policy.cycles_since_listen,
                self.slot_manager.listen_policy.listen_interval,
            ),
            active_qsos: self.active_qso_count,
            max_qsos: self.config.max_concurrent_qsos,
            idle_cycles: self.idle_cycles,
            band_name: self.band_strategy.current_band_name().to_string(),
            tx_offset_hz: self.config.tx_offset_hz,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_cq_message(text: &str) -> bool {
    let upper = text.to_uppercase();
    upper.starts_with("CQ ")
}

/// Returns `true` if `tok` looks like a report / RR73 / RRR / 73 payload —
/// the trailing token of a standard FT8 exchange (vs. a grid in a reply).
///
/// `pub` so callers that need the same "is this the payload of a committed
/// exchange" test (e.g. the TUI Callers panel's busy detector mirrors this
/// logic) have a single canonical definition.
pub fn is_exchange_payload(tok: &str) -> bool {
    let u = tok.to_uppercase();
    if u == "RR73" || u == "RRR" || u == "73" || u == "RR" {
        return true;
    }
    // Signal reports: "-12", "+05", "R-12", "R+05".
    let body = u.strip_prefix('R').unwrap_or(&u);
    if let Some(rest) = body.strip_prefix(['-', '+']) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// `true` if `tok` has exact Maidenhead locator shape: 2 letters + 2 digits
/// (a 4-character field+square), optionally followed by a 2-letter
/// subsquare suffix (6 characters total). Any other length, or wrong
/// character class in any position, is rejected — a 5-character or
/// wrong-shaped 6-character token (e.g. "FN42X", "FN4212") is not a grid.
fn looks_like_grid(tok: &str) -> bool {
    let chars: Vec<char> = tok.chars().collect();
    let field_square_ok = |c: &[char]| {
        c[0].is_ascii_alphabetic()
            && c[1].is_ascii_alphabetic()
            && c[2].is_ascii_digit()
            && c[3].is_ascii_digit()
    };
    match chars.len() {
        4 => field_square_ok(&chars),
        6 => {
            field_square_ok(&chars)
                && chars[4].is_ascii_alphabetic()
                && chars[5].is_ascii_alphabetic()
        }
        _ => false,
    }
}

/// Inspect a decoded message and, if it is a **third-party exchange** —
/// two callsign tokens followed by a report/RR73/RRR/73 payload, where it
/// is neither a CQ nor directed at/from us — return the participant
/// callsigns. These are the stations to mark "busy" so the autonomous
/// operator yields to a DX mid-QSO with someone else.
///
/// Returns an empty vec for CQs, messages involving `our_callsign`, replies
/// that carry a grid (not yet a committed exchange), and anything that does
/// not parse as a 3-token `<to> <from> <payload>` exchange.
///
/// `pub` so the TUI Callers panel's "BUSY" detector can share this exact
/// definition (it re-implements an equivalent locally because `pancetta-tui`
/// does not depend on `pancetta-qso`; keeping this canonical and tested
/// guards both copies against drift).
pub fn third_party_exchange_callsigns(text: &str, our_callsign: &str) -> Vec<String> {
    if is_cq_message(text) {
        return Vec::new();
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    // Standard exchange is exactly "<to> <from> <payload>".
    if parts.len() != 3 {
        return Vec::new();
    }
    let (to, from, payload) = (parts[0], parts[1], parts[2]);
    if !is_exchange_payload(payload) {
        // Grid replies ("CALL CALL FN42") or anything else: not a
        // committed exchange we should yield to.
        return Vec::new();
    }
    // If either party is us, this is our own QSO traffic, not a third party.
    if to.eq_ignore_ascii_case(our_callsign) || from.eq_ignore_ascii_case(our_callsign) {
        return Vec::new();
    }
    // Both tokens must look like callsigns (contain a digit) to avoid
    // treating tokens like "TNX" or directed-CQ words as callsigns.
    let looks_like_call =
        |s: &str| s.len() >= 3 && s.chars().any(|c| c.is_ascii_digit()) && s != "73";
    let mut calls = Vec::new();
    if looks_like_call(to) {
        calls.push(to.to_uppercase());
    }
    if looks_like_call(from) {
        calls.push(from.to_uppercase());
    }
    calls
}

fn extract_grid_from_cq(text: &str) -> Option<String> {
    // CQ messages: "CQ W1ABC FN42" or "CQ DX W1ABC FN42"
    let parts: Vec<&str> = text.split_whitespace().collect();
    // The grid is the last token if it looks like a Maidenhead locator.
    let last = parts.last()?;
    looks_like_grid(last).then(|| last.to_uppercase())
}

/// Simple deterministic jitter in ±200 Hz range using system time low bits.
fn simple_jitter() -> f64 {
    let nanos = Utc::now().timestamp_subsec_nanos();
    // Map to -200..+200 range.
    ((nanos % 401) as f64) - 200.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
// rationale: test-only builder structs assigned field-by-field after
// default(); sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_slot_parity_derivation() {
        // Slot 0 (t=0..14) -> Even
        assert_eq!(SlotParity::from_unix_secs(0), SlotParity::Even);
        assert_eq!(SlotParity::from_unix_secs(14), SlotParity::Even);
        // Slot 1 (t=15..29) -> Odd
        assert_eq!(SlotParity::from_unix_secs(15), SlotParity::Odd);
        assert_eq!(SlotParity::from_unix_secs(29), SlotParity::Odd);
        // Slot 2 (t=30..44) -> Even
        assert_eq!(SlotParity::from_unix_secs(30), SlotParity::Even);
    }

    #[test]
    fn test_listen_policy_backoff() {
        let config = ListenCycleConfig {
            initial_interval: 3,
            backoff_interval: 5,
            collision_interval: 2,
            backoff_threshold: 3,
        };
        let mut policy = ListenPolicy::new(&config);
        assert_eq!(policy.listen_interval, 3);

        // Record enough clean listens to trigger backoff.
        for _ in 0..3 {
            policy.record_clean_listen(&config);
        }
        assert_eq!(policy.listen_interval, 5);
    }

    #[test]
    fn test_listen_policy_collision() {
        let config = ListenCycleConfig::default();
        let mut policy = ListenPolicy::new(&config);

        policy.record_collision(&config);
        assert!(policy.collision_state);
        assert_eq!(policy.listen_interval, config.collision_interval);
        assert_eq!(policy.collision_cooldown, 10);
    }

    #[test]
    fn test_collision_detector_no_collision() {
        let detector = CollisionDetector::new(1500.0, 50.0);
        let messages = vec![DecodedMessageInfo {
            callsign: Some("K1DEF".into()),
            frequency_hz: 800.0,
            snr: -10,
            message_text: "CQ K1DEF FN31".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];

        let result = detector.check_for_collision(&messages);
        assert!(!result.detected);
    }

    #[test]
    fn test_collision_detector_collision() {
        let detector = CollisionDetector::new(1500.0, 50.0);
        let messages = vec![DecodedMessageInfo {
            callsign: Some("K1DEF".into()),
            frequency_hz: 1520.0,
            snr: -10,
            message_text: "CQ K1DEF FN31".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];

        let result = detector.check_for_collision(&messages);
        assert!(result.detected);
        assert_eq!(result.interfering_calls, vec!["K1DEF".to_string()]);
    }

    #[test]
    fn collision_jitter_resets_the_no_response_streak() {
        // Codex review (PR #276, round 6): without this, an idle Auto
        // operator that already reached the switch threshold — collision
        // jitter fires first and moves the offset — would still have a
        // streak sitting at/above the threshold, so the very next self-CQ
        // immediately re-enters the switch branch and moves away again
        // without ever trying the frequency collision avoidance just picked.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.tx_offset_hz = 1500.0;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.cq_no_response_streak = 10; // already past any reasonable threshold
                                       // Codex review (PR #277, round 2): `simple_jitter()` draws from real
                                       // system time (-200..=200 around tx_offset_hz=1500, i.e. always in
                                       // [1300, 1700]), and can occasionally return exactly 0 — which,
                                       // combined with the round-2 no-op guard, would leave `new_offset`
                                       // equal to the default `prev_offset` (also 1500, since
                                       // `current_cq_offset_hz` starts unset) about 1-in-401 runs, making
                                       // this assertion flaky. Seed `current_cq_offset_hz` to a value
                                       // jitter can never reach so `new_offset != prev_offset` holds no
                                       // matter what the real jitter draws, without weakening the test to
                                       // a synthetic clamp scenario.
        op.current_cq_offset_hz = Some(1000.0);
        // Codex review (PR #277, round 3): the streak reset is now also
        // gated on having escaped the interferer's collision tolerance
        // (not just "the offset moved at all"). With the interferer placed
        // near tx_offset_hz=1500 (required to trigger detection at all,
        // since the detector checks against tx_offset_hz) and jitter always
        // landing new_offset in [1300, 1700], a real jitter draw could
        // coincidentally still land within 50 Hz of the interferer,
        // flaking this assertion. Decouple detection from the jitter base
        // by pointing the detector's own tracked offset at a frequency far
        // from tx_offset_hz's jitter range, so every possible new_offset
        // guarantees escape regardless of the real jitter draw.
        op.collision_detector.our_tx_offset_hz = 700.0;

        let messages = vec![DecodedMessageInfo {
            callsign: Some("K1DEF".into()),
            frequency_hz: 710.0, // within the 50 Hz collision tolerance of 700 (detector's offset)
            snr: -10,
            message_text: "CQ K1DEF FN31".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        let actions = op.process_collision_listen(&messages);

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "sanity: collision jitter must have fired"
        );
        assert_eq!(
            op.cq_no_response_streak, 0,
            "the streak must reset when collision avoidance moves the frequency"
        );
    }

    #[test]
    fn collision_jitter_preserves_streak_when_the_new_offset_is_still_within_collision_range() {
        // Codex review (PR #277, round 3): a nonzero jitter can still land
        // within the collision detector's tolerance of the same interferer
        // (e.g. a small draw). `simple_jitter()` has no test injection
        // point, so this asserts the actual gating INVARIANT — whichever
        // way any given real jitter draw falls — across many trials rather
        // than depending on one draw landing in either bucket, giving
        // deterministic coverage of both branches of the new
        // `still_colliding` check.
        for _ in 0..100 {
            let mut config = AutonomousConfig::default();
            config.enabled = true;
            config.tx_offset_hz = 1500.0;
            let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
            op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Auto.as_u8(),
            )));
            op.cq_no_response_streak = 10;
            op.current_cq_offset_hz = Some(1000.0); // outside jitter's [1300,1700] range

            let messages = vec![DecodedMessageInfo {
                callsign: Some("K1DEF".into()),
                frequency_hz: 1520.0, // within the 50 Hz collision tolerance of 1500
                snr: -10,
                message_text: "CQ K1DEF FN31".into(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }];
            op.process_collision_listen(&messages);

            let new_offset = op.current_cq_offset_hz.expect("jitter always sets this");
            let still_colliding = (1520.0_f64 - new_offset).abs() <= 50.0;
            if still_colliding {
                assert_eq!(
                    op.cq_no_response_streak, 10,
                    "new_offset {new_offset} is still within 50 Hz of the 1520 Hz \
                     interferer — the streak must NOT reset"
                );
            } else {
                assert_eq!(
                    op.cq_no_response_streak, 0,
                    "new_offset {new_offset} escaped the 1520 Hz interferer's \
                     collision range — the streak must reset"
                );
            }
        }
    }

    #[test]
    fn collision_jitter_preserves_streak_when_the_offset_does_not_actually_change() {
        // Codex review (PR #277, round 1): `simple_jitter()` can return 0,
        // and at the 200/2800 Hz boundaries an outward jitter clamps back to
        // the current offset — in either case no real collision avoidance
        // happened, so the streak must survive and no FrequencyShift should
        // be reported. Force a deterministic no-op regardless of the actual
        // jitter value by seeding `tx_offset_hz` at 3000 Hz: any jitter in
        // simple_jitter()'s -200..+200 range pushes the raw value to
        // 2800..3200, which always clamps to exactly 2800 Hz — matching the
        // sticky offset already recorded below.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.tx_offset_hz = 3000.0;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.cq_no_response_streak = 10;
        op.current_cq_offset_hz = Some(2800.0);

        let messages = vec![DecodedMessageInfo {
            callsign: Some("K1DEF".into()),
            frequency_hz: 3020.0,
            snr: -10,
            message_text: "CQ K1DEF FN31".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        let actions = op.process_collision_listen(&messages);

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "no FrequencyShift should be reported when the clamped offset didn't change"
        );
        assert_eq!(
            op.cq_no_response_streak, 10,
            "a no-op jitter must not reset the no-response streak"
        );
    }

    #[test]
    fn test_is_cq_message() {
        assert!(is_cq_message("CQ W1ABC FN42"));
        assert!(is_cq_message("CQ DX W1ABC FN42"));
        assert!(is_cq_message("cq w1abc fn42"));
        assert!(!is_cq_message("W1ABC K1DEF -15"));
        assert!(!is_cq_message(""));
    }

    #[test]
    fn test_extract_grid_from_cq() {
        assert_eq!(extract_grid_from_cq("CQ W1ABC FN42"), Some("FN42".into()));
        assert_eq!(
            extract_grid_from_cq("CQ DX W1ABC FN42AB"),
            Some("FN42AB".into())
        );
        assert_eq!(extract_grid_from_cq("CQ W1ABC"), None);
    }

    fn op_for_directed_response_tests() -> AutonomousOperator {
        AutonomousOperator::new(AutonomousConfig::default(), "W1ABC".into(), None)
    }

    #[test]
    fn is_directed_response_accepts_grid_reply() {
        // The standard FIRST reply to a CQ: a grid, not a report yet.
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("W1ABC K9ZZ EM48"));
    }

    #[test]
    fn is_directed_response_accepts_bare_grid_less_response() {
        // Codex review (PR #276): the Type-4 compound-callsign encoder
        // deliberately drops the grid, so a real CqResponse can be just two
        // callsigns — `MessageType::CqResponse { grid: None, .. }`.
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("W1ABC K9ZZ"));
    }

    #[test]
    fn is_directed_response_accepts_report_and_rr73() {
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("W1ABC K9ZZ -05"));
        assert!(op.is_directed_response("W1ABC K9ZZ R-05"));
        assert!(op.is_directed_response("W1ABC K9ZZ RR73"));
        assert!(op.is_directed_response("W1ABC K9ZZ 73"));
    }

    #[test]
    fn is_directed_response_accepts_hash_rendered_addressee() {
        // Codex review (PR #276): an i3=4 hash-rendered callsign ("<W1ABC>")
        // in the addressee position — the DX has previously cached our
        // callsign and is compressing it on a later rung.
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("<W1ABC> K9ZZ -05"));
    }

    #[test]
    fn is_directed_response_accepts_hash_rendered_addressee_with_compound_replier() {
        // Hash-rendered addressee (us) combined with a compound-call
        // replier — both edge cases in one message.
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("<W1ABC> YS/WE9G"));
    }

    #[test]
    fn is_directed_response_rejects_our_callsign_in_replier_position() {
        // Codex review (PR #276, round 2): `calling_station` (first token)
        // is the addressee; `responding_station` (second token) is the
        // replier. A message where OUR callsign is the replier and someone
        // else is the addressee is a message where WE supposedly answered
        // a DIFFERENT station's CQ — not something a genuine RX decode of
        // a reply TO us should ever produce. An earlier version of this
        // classifier incorrectly accepted this by checking both fields.
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("YS/WE9G <W1ABC>"));
        assert!(!op.is_directed_response("YS/WE9G W1ABC"));
    }

    #[test]
    fn is_directed_response_rejects_not_directed_at_us() {
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("K1DEF K9ZZ -05"));
    }

    #[test]
    fn is_directed_response_rejects_our_own_cq() {
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("CQ W1ABC FN42"));
    }

    #[test]
    fn is_directed_response_rejects_bare_callsign() {
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("W1ABC"));
    }

    #[test]
    fn is_directed_response_rejects_free_text_starting_with_our_callsign() {
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("W1ABC TEST MESSAGE"));
        assert!(!op.is_directed_response("W1ABC HELLO THERE"));
    }

    #[test]
    fn is_directed_response_rejects_malformed_sender_token() {
        let op = op_for_directed_response_tests();
        // "from" token doesn't look like a callsign (no digit).
        assert!(!op.is_directed_response("W1ABC HELLO -05"));
        // No letter at all.
        assert!(!op.is_directed_response("W1ABC 123 -05"));
        // Contains a non-alphanumeric character.
        assert!(!op.is_directed_response("W1ABC XX1! -05"));
    }

    #[test]
    fn is_directed_response_rejects_malformed_grid_payload() {
        let op = op_for_directed_response_tests();
        // 5 characters: not a valid 4- or 6-char Maidenhead locator.
        assert!(!op.is_directed_response("W1ABC K9ZZ FN42X"));
        // 6 characters but wrong shape (digits, not letters, in positions 5-6).
        assert!(!op.is_directed_response("W1ABC K9ZZ FN4212"));
        // Valid 6-char subsquare grid IS accepted.
        assert!(op.is_directed_response("W1ABC K9ZZ FN42ab"));
    }

    #[test]
    fn is_directed_response_accepts_compound_and_portable_senders() {
        // Prefix-portable and suffix-portable are real, tested reply forms
        // (adversarial_compound_calls.rs) — must not be rejected as
        // malformed just because they contain '/'.
        let op = op_for_directed_response_tests();
        assert!(op.is_directed_response("W1ABC VP2/W1XYZ FK87"));
        assert!(op.is_directed_response("W1ABC K1ABC/R -05"));
        assert!(op.is_directed_response("W1ABC EA8/G8BCG RR73"));
    }

    #[test]
    fn is_directed_response_rejects_malformed_compound_modifiers() {
        // The real callsign validator rejects a garbage `/`-separated
        // component (not a short prefix/suffix modifier) even when another
        // component looks like a valid base call.
        let op = op_for_directed_response_tests();
        assert!(!op.is_directed_response("W1ABC W1XYZ/TOOLONG -05"));
        assert!(!op.is_directed_response("W1ABC BOGUS/W1XYZ/INVALID -05"));
    }

    #[test]
    fn test_slot_manager_auto_detect() {
        let config = ListenCycleConfig::default();
        let mut sm = SlotManager::new(SlotParityConfig::Auto, &config);
        assert!(sm.our_slot.is_none());

        // Feed activity: even slots quiet, odd slots busy.
        sm.record_slot_activity(SlotParity::Even, 2);
        sm.record_slot_activity(SlotParity::Odd, 10);
        sm.record_slot_activity(SlotParity::Even, 1);
        sm.record_slot_activity(SlotParity::Odd, 8);

        // After 4 slots, should pick Even (quieter).
        assert_eq!(sm.our_slot, Some(SlotParity::Even));
    }

    #[test]
    fn autonomous_config_default_cq_no_response_switch_after_is_5() {
        let config = AutonomousConfig::default();
        assert_eq!(config.cq_no_response_switch_after, 5);
    }

    #[test]
    fn allocate_smart_frequency_avoids_the_given_offset() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });

        let chosen = op.allocate_smart_frequency(None, None, Some(1500.0));

        assert!(
            (chosen - 1500.0).abs() >= 75.0,
            "expected a frequency at least 75 Hz from the avoided 1500 Hz, got {chosen}"
        );
    }

    #[test]
    fn allocate_smart_frequency_hard_excludes_even_when_avoid_hz_scores_best() {
        // Codex review (PR #276, round 6): `own_frequencies`' -50 soft
        // penalty isn't guaranteed to displace avoid_hz from first place on
        // a sufficiently crowded band ("effectively eliminates" per its own
        // comment is aspirational, not guaranteed). Occupy densely
        // everywhere except a narrow window around 1500 Hz, so 1500 Hz is
        // the ONLY genuinely clear spot on the whole band (would win even
        // after -50) — confirm it's still hard-excluded.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });

        // Codex review (PR #277, round 2): the original `> 100.0` threshold
        // left a 200 Hz-wide clear window (1400-1600) around 1500 — wider
        // than the 75 Hz exclusion radius under test — so genuinely clear,
        // non-excluded candidates already existed at the 75/100 Hz boundary
        // (e.g. 1425/1575) and could win on score alone, without the hard
        // filter doing any work. Narrow the clear window to entirely inside
        // the exclusion radius (`> 50.0`, i.e. only [1450, 1550] stays
        // clear) so nothing outside the excluded band is naturally clear.
        let mut records = Vec::new();
        let mut freq = 200.0f64;
        while freq <= 2800.0 {
            if (freq - 1500.0).abs() > 50.0 {
                records.push(DecodeRecord {
                    frequency_hz: freq,
                    time_slot: TimeSlot::First,
                });
                records.push(DecodeRecord {
                    frequency_hz: freq,
                    time_slot: TimeSlot::Second,
                });
            }
            freq += 50.0;
        }
        op.decode_history.push_cycle(records);

        // Confirm the premise directly against the unfiltered ranker: with
        // this occupancy, the top-ranked candidate (no avoid_hz filtering
        // applied at all) really does fall inside the 75 Hz exclusion
        // radius around 1500 — proving the fix's hard filter is doing real
        // work below, not just re-confirming what the ranker would have
        // picked anyway.
        let unfiltered_top = op
            .smart_allocator
            .rank_candidates_with_parity(
                op.spectral_snapshot.as_ref().unwrap(),
                &op.decode_history,
                &[],
                None,
                None,
            )
            .into_iter()
            .next()
            .expect("ranker must return at least one candidate");
        assert!(
            (unfiltered_top.offset_hz - 1500.0).abs() < 75.0,
            "test premise broken: the unfiltered ranker's top pick ({}) must fall \
             inside the exclusion radius for this to be a meaningful regression test",
            unfiltered_top.offset_hz
        );

        let chosen = op.allocate_smart_frequency(None, None, Some(1500.0));

        assert!(
            (chosen - 1500.0).abs() >= 75.0,
            "avoid_hz must be hard-excluded even when it's the only clear spot \
             on an otherwise crowded band, got {chosen}"
        );
    }

    #[test]
    fn allocate_smart_frequency_returns_avoid_hz_unchanged_when_the_hard_filter_empties_out() {
        // Codex review (PR #277, round 3): if `min_separation_hz` is
        // configured large enough that every ranked candidate falls inside
        // the exclusion radius, the hard filter empties the candidate list.
        // Round 2's "farthest candidate" fallback still openly violated the
        // configured separation; the caller (`decide_at`'s switch path)
        // must instead be able to tell "no valid relocation exists" apart
        // from a real pick. Signal that by returning `avoid_hz` itself
        // unchanged, distinct from both the farthest-candidate compromise
        // and the avoid_hz-blind legacy allocator's fallback (which, with
        // no own/observed frequencies recorded, always returns the
        // allocation range's minimum — 200 Hz here, not 1000 Hz).
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.frequency.min_separation_hz = 10_000.0;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });

        let chosen = op.allocate_smart_frequency(None, None, Some(1000.0));

        assert_eq!(
            chosen, 1000.0,
            "must return avoid_hz unchanged (a detectable no-op) rather than the \
             avoid_hz-blind legacy allocator's 200.0 or a farther-but-still-excluded compromise"
        );
    }

    #[test]
    fn allocate_smart_frequency_ignores_avoid_hz_in_hold_mode() {
        // Hold mode returns the pinned/parked offset regardless of avoid_hz —
        // avoid_hz only matters on the Auto ranking path.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.tx_offset_hz = 1500.0;
        let op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        // Default mode is Hold (no set_tx_freq_mode_source call).
        let chosen = op.allocate_smart_frequency(None, None, Some(1500.0));
        assert_eq!(chosen, 1500.0);
    }

    fn primed_operator(
        cq_after_idle_cycles: u32,
        switch_after: u32,
        auto: bool,
    ) -> AutonomousOperator {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = cq_after_idle_cycles;
        config.cq_no_response_switch_after = switch_after;
        config.listen_cycle.initial_interval = 100; // never listen-jitter mid-test

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        if auto {
            op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Auto.as_u8(),
            )));
        }
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        op
    }

    fn run_cq_rounds(op: &mut AutonomousOperator, rounds: u32) -> Vec<Vec<OperatorAction>> {
        let even_ts: i64 = 0;
        let mut all = Vec::new();
        for _ in 0..rounds {
            // cq_after_idle_cycles = 2: one "idle" tick, then one "CQ" tick.
            op.decide_at(even_ts);
            all.push(op.decide_at(even_ts));
        }
        all
    }

    #[test]
    fn switch_skips_and_listens_when_no_frequency_honors_the_exclusion() {
        // Codex review (PR #277, round 3): when min_separation_hz is
        // configured too large for any candidate to honor the exclusion,
        // allocate_smart_frequency returns avoid_hz unchanged. Committing to
        // that as if it were a real switch would reset the streak and
        // transmit right back on the abandoned frequency. This must instead
        // fall back to the same "skip the window and listen" path as the
        // stale-occupancy-data case, preserving the streak.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 2;
        config.cq_no_response_switch_after = 3;
        config.listen_cycle.initial_interval = 100;
        config.frequency.min_separation_hz = 10_000.0; // impossible to honor

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }

        let rounds = run_cq_rounds(&mut op, 4);

        for (i, round) in rounds.iter().enumerate() {
            assert!(
                !round
                    .iter()
                    .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
                "round {i}: must never report a FrequencyShift when no candidate honors \
                 the exclusion, got {round:?}"
            );
        }
        // The threshold round (index 3, 0-based — matching
        // auto_mode_switches_frequency_after_streak_threshold's established
        // "4th round" cadence for switch_after=3) must listen instead of
        // transmitting.
        assert!(
            rounds[3]
                .iter()
                .any(|a| matches!(a, OperatorAction::Listen)),
            "expected the threshold round to skip and listen, got {:?}",
            rounds[3]
        );
        assert!(
            !rounds[3]
                .iter()
                .any(|a| matches!(a, OperatorAction::Transmit { .. })),
            "must not transmit on the round where the switch is skipped, got {:?}",
            rounds[3]
        );
        assert_eq!(
            op.cq_no_response_streak, 3,
            "the streak must be preserved (not reset, not incremented further) while \
             the switch keeps being skipped"
        );
    }

    #[test]
    fn auto_mode_switches_frequency_after_streak_threshold() {
        let mut op = primed_operator(2, 3, true);
        // Pre-fill decode history so freshness never blocks this test.
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }

        let rounds = run_cq_rounds(&mut op, 4);

        // First 3 CQ rounds: no FrequencyShift.
        for round in &rounds[..3] {
            assert!(
                !round
                    .iter()
                    .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
                "must not switch before the threshold"
            );
        }
        // 4th round: FrequencyShift AND a CQ transmit on the new offset.
        let shift_offset = rounds[3].iter().find_map(|a| {
            if let OperatorAction::FrequencyShift { new_offset_hz } = a {
                Some(*new_offset_hz)
            } else {
                None
            }
        });
        assert!(
            shift_offset.is_some(),
            "expected a FrequencyShift at the threshold round"
        );
        let tx_offset = rounds[3].iter().find_map(|a| {
            if let OperatorAction::Transmit {
                frequency_offset,
                message_text,
                ..
            } = a
            {
                message_text.starts_with("CQ").then_some(*frequency_offset)
            } else {
                None
            }
        });
        assert_eq!(
            tx_offset, shift_offset,
            "the CQ must go out on the newly-switched frequency"
        );
    }

    #[test]
    fn auto_mode_stays_on_switched_offset_across_subsequent_routine_cqs() {
        // Codex review (PR #276): the switched-to offset must remain the CQ
        // baseline for the next streak, not be discarded after one
        // transmission — the allocator has no memory of "we chose to leave
        // the old spot," so a routine (non-switching) re-rank on an
        // unchanged, still-clear band would otherwise just pick the
        // abandoned frequency straight back.
        let mut op = primed_operator(2, 4, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }

        let rounds = run_cq_rounds(&mut op, 7);

        // switch_after=4: entering streak is 0,1,2,3,4 on rounds 1-5 — the
        // switch fires on round 5 (index 4), when entering streak first
        // reaches the threshold. (The streak keeps incrementing even on a
        // switch round — the switch CQ is itself unanswered so far too —
        // so round 6 doesn't yet re-trip the threshold: pick a threshold
        // high enough that rounds 6-7 are unambiguously still routine.)
        let shift_offset = rounds[4].iter().find_map(|a| {
            if let OperatorAction::FrequencyShift { new_offset_hz } = a {
                Some(*new_offset_hz)
            } else {
                None
            }
        });
        assert!(
            shift_offset.is_some(),
            "expected a FrequencyShift at round 5 (switch_after=4)"
        );

        // Rounds 6 and 7: routine CQs, no further switch — must keep using
        // the offset from round 5, not revert to the pre-switch frequency.
        for round in &rounds[5..] {
            assert!(
                !round
                    .iter()
                    .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
                "must not switch again immediately"
            );
            let tx_offset = round.iter().find_map(|a| {
                if let OperatorAction::Transmit {
                    frequency_offset,
                    message_text,
                    ..
                } = a
                {
                    message_text.starts_with("CQ").then_some(*frequency_offset)
                } else {
                    None
                }
            });
            assert_eq!(
                tx_offset, shift_offset,
                "routine CQ after a switch must stay on the switched offset, not revert"
            );
        }
    }

    #[test]
    fn hold_mode_never_switches_even_past_the_streak_threshold() {
        let mut op = primed_operator(2, 2, false); // Hold mode: no set_tx_freq_mode_source call
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }

        let rounds = run_cq_rounds(&mut op, 6); // well past the threshold of 2

        for round in &rounds {
            assert!(
                !round
                    .iter()
                    .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
                "Hold mode must never emit a FrequencyShift"
            );
        }
    }

    #[test]
    fn auto_mode_listens_instead_of_switching_when_history_is_thin() {
        // switch_after = 1 (a reachable, config-valid threshold — 0 is
        // rejected by AutonomousConfig::validate_section): the first
        // CQ-eligible call sends a real no-response CQ (streak -> 1); the
        // second is where the streak first meets the threshold, and is the
        // one that must be gated on thin history.
        let mut op = primed_operator(1, 1, true);
        // Deliberately do NOT prime decode history — cycles_recorded() stays
        // below FrequencyAllocatorConfig::default().decode_history_cycles (4).

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle_cycles 0->1 >= cq_after_idle_cycles(1): first CQ, streak -> 1
        let actions = op.decide_at(even_ts); // streak(1) >= switch_after(1): would-be switch, history thin

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, OperatorAction::Transmit { .. })),
            "must not transmit a blind CQ/switch while history is thin"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "must not switch while history is thin"
        );
        assert!(actions.iter().any(|a| matches!(a, OperatorAction::Listen)));
    }

    #[test]
    fn streak_resets_on_directed_reply_not_on_active_qso_count() {
        // Regression: our own self-CQ opens a CallingCq QSO, which is itself
        // "active" until it times out — active_qso_count is NOT a safe reset
        // signal (it would zero the streak against our own unanswered CQ,
        // not a real response). Only a decoded reply directed at us must
        // reset it.
        let mut op = primed_operator(2, 2, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1

        // active_qso_count alone must NOT reset the streak (this is exactly
        // what our own not-yet-timed-out self-CQ looks like from outside).
        op.set_active_qso_count(1);
        op.decide_at(even_ts); // self-CQ branch doesn't run (active_qso_count > 0), no reset either
        op.set_active_qso_count(0);

        // A genuine directed reply ("<us> <them> <payload>") DOES reset it.
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("K9ZZ".into()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "W1ABC K9ZZ -05".into(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }],
            &NullDxEvaluator,
        );

        op.decide_at(even_ts); // idle
        let round = op.decide_at(even_ts); // CQ #2 post-reset, streak -> 1 again, not 3
        assert!(
            !round
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "streak must have reset on the directed reply, not kept accumulating"
        );
    }

    #[test]
    fn streak_is_not_reset_by_active_qso_count_alone() {
        // The precise regression Codex flagged: active_qso_count > 0 with NO
        // directed reply decoded must leave the streak untouched.
        let mut op = primed_operator(2, 2, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1

        op.set_active_qso_count(1); // e.g. our own pending self-CQ's CallingCq QSO
        op.decide_at(even_ts); // must NOT reset the streak
        op.set_active_qso_count(0);

        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #2: streak entering=1 (NOT reset to 0), 1 < 2, -> becomes 2
        op.decide_at(even_ts); // idle
        let round = op.decide_at(even_ts); // CQ #3: streak entering=2 >= threshold(2) -> switches
        assert!(
            round
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "streak must have kept accumulating across the active_qso_count blip, reaching \
             the threshold on the 3rd CQ"
        );
    }

    #[test]
    fn restore_cq_state_undoes_a_suppressed_routine_cq() {
        // decide_at counts a self-CQ optimistically, before the
        // coordinator's TX gates run, snapshotting internally right before
        // that mutation. restore_cq_state() is what the coordinator calls
        // when a gate suppressed it.
        let mut op = primed_operator(2, 5, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1 (speculative, self-snapshotted)
        op.restore_cq_state(); // gate suppressed it: streak -> back to 0

        op.decide_at(even_ts); // idle
        let round = op.decide_at(even_ts); // CQ #2 (post-restore): entering streak 0, not 1
        assert!(
            !round
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "restoring a suppressed CQ must roll the streak back, not just pause it"
        );
    }

    #[test]
    fn restore_cq_state_fully_undoes_a_suppressed_switch() {
        // Codex review (PR #276, round 3): a suppressed SWITCH mutates more
        // than the counter — it resets the streak, installs a new sticky
        // offset, and updates config.tx_offset_hz. A bare "subtract one"
        // would leave the station "switched" to a frequency it never
        // actually transmitted on, with the real pre-switch streak lost.
        // snapshot/restore must undo all of it.
        let mut op = primed_operator(2, 3, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        // Build up a real streak just below the switch threshold.
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #2, streak -> 2

        // switch_after=3: entering streak is 2 at CQ #3, still below
        // threshold — confirms it's routine before CQ #4 below switches.
        op.decide_at(even_ts); // idle
        let round3 = op.decide_at(even_ts); // CQ #3, routine
        assert!(
            !round3
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "sanity: CQ #3 must be routine, not yet a switch (test setup check)"
        );

        let pre_switch_streak = op.cq_no_response_streak;
        let pre_switch_offset = op.current_cq_offset_hz;
        assert_eq!(
            pre_switch_streak, 3,
            "sanity: streak should be 3 entering CQ #4"
        );

        op.decide_at(even_ts); // idle
        let switch_round = op.decide_at(even_ts); // CQ #4: entering streak 3 >= 3 -> switches
        assert!(
            switch_round
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "sanity: CQ #4 must be the switch (test setup check)"
        );

        // The switch already ran (streak reset+incremented, offset changed).
        // Now simulate the coordinator discovering it was suppressed.
        op.restore_cq_state();

        assert_eq!(
            op.cq_no_response_streak, pre_switch_streak,
            "streak must be restored to its real pre-switch value, not left at \
             whatever the switch's reset+increment produced"
        );
        assert_eq!(
            op.current_cq_offset_hz, pre_switch_offset,
            "sticky offset must be restored — the switch never actually transmitted"
        );
    }

    #[test]
    fn band_hop_clears_stale_frequency_space_data() {
        // Codex review (PR #276): decode_history/spectral_snapshot/
        // current_cq_offset_hz are scoped to the OLD band's audio-offset
        // space — a dial change must invalidate them, or the no-response
        // streak's freshness check (and the ordinary allocator) would rank
        // the new band using old-band data.
        //
        // `decide_at` processes band-hop (Step 0) and the self-CQ/switch
        // decision (Step 4) in the SAME cycle, so this test forces the
        // streak already past the switch threshold: if the clearing didn't
        // work, this cycle would both hop AND switch using the stale
        // (pre-hop) data. With clearing working, the freshness check sees
        // 0 recorded cycles and takes the Listen-instead-of-switch path —
        // which also means `current_cq_offset_hz` is never repopulated
        // within this same cycle, so it stays cleared afterward too.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 1;
        config.cq_no_response_switch_after = 1;
        config.listen_cycle.initial_interval = 1000;
        config.band_hopping = BandHoppingConfig {
            enabled: true,
            hop_threshold: 1,
            bands: vec![
                BandEntry {
                    dial_frequency: 14_074_000,
                    band_name: "20m".into(),
                    priority: 1,
                },
                BandEntry {
                    dial_frequency: 7_074_000,
                    band_name: "40m".into(),
                    priority: 2,
                },
            ],
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        // One zero-activity cycle meets hop_threshold=1, and also gives
        // decode_history something to have been set from, so clearing is
        // actually observable.
        op.feed_decoded_messages(&[], &NullDxEvaluator);
        op.current_cq_offset_hz = Some(1500.0);
        op.cq_no_response_streak = 5; // already past switch_after=1

        assert!(op.spectral_snapshot.is_some());
        assert!(op.decode_history.cycles_recorded() > 0);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, OperatorAction::ChangeBand { .. })),
            "expected a band hop with hop_threshold=1"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "must NOT switch using stale pre-hop occupancy data"
        );
        assert!(
            actions.iter().any(|a| matches!(a, OperatorAction::Listen)),
            "expected the freshness-gated Listen-instead-of-switch path"
        );
        assert!(
            op.spectral_snapshot.is_none(),
            "spectral snapshot must be cleared on band hop"
        );
        assert_eq!(
            op.decode_history.cycles_recorded(),
            0,
            "decode history must be cleared on band hop"
        );
        assert!(
            op.current_cq_offset_hz.is_none(),
            "current CQ offset must be cleared on band hop and not repopulated \
             by a same-cycle switch that correctly declined to use stale data"
        );
    }

    #[test]
    fn restore_cq_state_does_not_undo_a_same_cycle_band_hop() {
        // Codex review (PR #276, round 4): the snapshot restore_cq_state()
        // applies must be taken AFTER Step 0's band-hop clearing, not from
        // before decide_at ran at all — otherwise "restoring" a suppressed
        // self-CQ on a band-hop cycle would bring back the stale PRE-hop
        // current_cq_offset_hz, undoing what the hop correctly invalidated.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 1;
        config.cq_no_response_switch_after = 100; // stay routine, not a switch
        config.listen_cycle.initial_interval = 1000;
        config.band_hopping = BandHoppingConfig {
            enabled: true,
            hop_threshold: 1,
            bands: vec![
                BandEntry {
                    dial_frequency: 14_074_000,
                    band_name: "20m".into(),
                    priority: 1,
                },
                BandEntry {
                    dial_frequency: 7_074_000,
                    band_name: "40m".into(),
                    priority: 2,
                },
            ],
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        op.feed_decoded_messages(&[], &NullDxEvaluator); // meets hop_threshold=1
        let stale_pre_hop_offset = 1500.0;
        op.current_cq_offset_hz = Some(stale_pre_hop_offset);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, OperatorAction::ChangeBand { .. })),
            "expected a band hop with hop_threshold=1"
        );
        // Not a switch (switch_after=100) — a routine CQ ran this cycle,
        // re-ranking fresh (current_cq_offset_hz was cleared by the hop)
        // and setting a NEW value. Simulate the coordinator finding it
        // suppressed by a downstream gate.
        op.restore_cq_state();

        assert_ne!(
            op.current_cq_offset_hz,
            Some(stale_pre_hop_offset),
            "restoring a suppressed same-cycle-band-hop CQ must NOT bring back \
             the stale pre-hop offset"
        );
    }

    #[test]
    fn restore_cq_state_does_not_undo_the_hold_mode_invalidation() {
        // Codex review (PR #276, round 5): the same class of bug as the
        // band-hop case above, but for the Hold-mode invalidation — that
        // clear must ALSO happen before the snapshot is taken, or restoring
        // a suppressed Hold-mode CQ brings back a stale pre-Hold Auto-mode
        // offset.
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 1;
        config.listen_cycle.initial_interval = 1000;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode.clone());
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        let stale_auto_offset = 1500.0;
        op.current_cq_offset_hz = Some(stale_auto_offset);

        // Switch to Hold before this cycle's self-CQ.
        mode.store(
            pancetta_core::TxFreqMode::Hold.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // Hold-mode CQ: current_cq_offset_hz invalidated
                               // (before the snapshot) and left at None.
        op.restore_cq_state(); // simulate the coordinator finding it suppressed

        assert_ne!(
            op.current_cq_offset_hz,
            Some(stale_auto_offset),
            "restoring a suppressed Hold-mode CQ must NOT bring back the stale \
             pre-Hold Auto-mode offset"
        );
    }

    #[test]
    fn restore_cq_state_for_attempt_matches_restore_cq_state_for_the_current_attempt() {
        // PAN-38: a downstream dispatch failure (QsoManager::start_cq
        // returns Err after every pre-dispatch gate already permitted the
        // self-CQ) must roll back the streak/offset exactly like an
        // explicit pre-dispatch suppression (restore_cq_state) —
        // restore_cq_state_for_attempt is the same underlying mechanism,
        // additionally gated on the attempt_id the coordinator echoes back
        // on the failure signal.
        let mut op = primed_operator(2, 5, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1 (speculative, self-snapshotted)
        let attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");
        op.restore_cq_state_for_attempt(attempt_id); // simulate a downstream dispatch failure

        op.decide_at(even_ts); // idle
        let round = op.decide_at(even_ts); // CQ #2 (post-restore): entering streak 0, not 1
        assert!(
            !round
                .iter()
                .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })),
            "restoring via the attempt-id path must roll the streak back \
             exactly like restore_cq_state(), not just pause it"
        );
    }

    #[test]
    fn restore_cq_state_for_attempt_does_not_resurrect_an_offset_a_later_generation_bump_invalidated(
    ) {
        // PAN-38 round 2 (Codex): a downstream dispatch-failure report for an
        // attempt that is STILL the latest snapshot (no newer self-CQ has
        // superseded it) used to unconditionally restore the snapshot's
        // sticky offset via restore_cq_state's full restore. But an
        // intervening decide_at cycle's own Hold/Auto-transition generation
        // check (PAN-39) can have already (correctly) cleared
        // current_cq_offset_hz to None in between the snapshot and the
        // restore -- the old unconditional restore would silently
        // resurrect the stale pre-transition value, undoing that correct
        // invalidation.
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 2;
        config.listen_cycle.initial_interval = 100;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode.clone());
        op.set_tx_freq_mode_generation_source(generation.clone());
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle 1, establishes the operator's baseline generation
        op.decide_at(even_ts); // idle 2 -> CQ #1 fires, streak -> 1, snapshot taken
        let attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");
        assert!(
            op.current_cq_offset_hz.is_some(),
            "CQ #1 must have picked a sticky offset"
        );

        // A Hold entry+exit happens entirely between polls -- invisible to a
        // direct mode-atomic read, same as PAN-39's scenario.
        generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        op.decide_at(even_ts); // idle cycle: observes the bump, clears the sticky offset
        assert!(
            op.current_cq_offset_hz.is_none(),
            "the generation bump must have invalidated the sticky offset already"
        );
        // No new self-CQ has fired (idle cycle only) -- attempt_id is still
        // the latest unresolved snapshot.
        assert_eq!(op.last_cq_attempt_id(), Some(attempt_id));

        // A late downstream dispatch-failure report for CQ #1 arrives now.
        op.restore_cq_state_for_attempt(attempt_id);

        assert!(
            op.current_cq_offset_hz.is_none(),
            "restoring a stale attempt must not resurrect an offset a later \
             generation bump already (correctly) invalidated"
        );
    }

    #[test]
    fn restore_cq_state_still_undoes_the_streak_when_only_the_offset_was_invalidated() {
        // PAN-38 round 4 (Codex): round 3's fix gated the ENTIRE restore
        // (streak AND offset) on one shared generation, which over-blocked
        // a safe streak restore whenever ONLY the offset had been
        // invalidated since the snapshot (a Hold/Auto transition or band
        // hop, neither of which touches the streak) -- silently
        // under-counting a suppressed attempt's speculative "+1"
        // permanently. The streak and offset generations are now tracked
        // independently, so an offset-only invalidation must still let the
        // streak roll back correctly.
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 2;
        config.listen_cycle.initial_interval = 100;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode.clone());
        op.set_tx_freq_mode_generation_source(generation.clone());
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle 1, establishes the baseline generation
        op.decide_at(even_ts); // idle 2 -> CQ #1 fires, streak -> 1, snapshot taken
        let attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");

        // A Hold entry+exit happens entirely between polls -- invalidates
        // the offset (PAN-39) but has no bearing on the streak.
        generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        op.decide_at(even_ts); // idle cycle: observes the bump, clears the sticky offset
        assert!(
            op.current_cq_offset_hz.is_none(),
            "the generation bump must have invalidated the sticky offset"
        );
        assert_eq!(
            op.last_cq_attempt_id(),
            Some(attempt_id),
            "no new self-CQ fired (idle cycle only) -- CQ #1's snapshot is still live"
        );

        // The coordinator determines CQ #1 was suppressed before reaching
        // the radio and calls the restore path.
        op.restore_cq_state_for_attempt(attempt_id);

        assert_eq!(
            op.cq_no_response_streak, 0,
            "the streak must still roll back to its pre-attempt value -- an offset-only \
             invalidation has no bearing on whether the streak restore is safe"
        );
        assert!(
            op.current_cq_offset_hz.is_none(),
            "the offset must remain uninvolved -- restoring it would resurrect the stale \
             pre-transition value the generation bump already (correctly) cleared"
        );
    }

    #[test]
    fn restore_cq_state_for_attempt_bounded_compensates_a_stale_non_switching_attempt_id() {
        // PAN-38 round 1 (Codex): a failure report for an attempt a LATER
        // self-CQ has since superseded used to be a complete no-op — this
        // can happen because the failure signal round-trips through the
        // message bus and the QSO component's own task, so it isn't
        // guaranteed to arrive before the next decide_at cycle takes a fresh
        // snapshot. That silently baked the failed attempt's speculative
        // "+1 streak" into the newer attempt's baseline forever. Since
        // neither CQ #1 nor CQ #2 triggered a frequency switch, the bounded
        // compensation path applies: decrementing the CURRENT streak by
        // exactly 1 undoes CQ #1's contribution without touching CQ #2's own
        // (still fully live) state — NOT a full restore to CQ #1's
        // pre-attempt values, which would incorrectly discard CQ #2's own
        // legitimate increment too.
        let mut op = primed_operator(2, 5, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1
        let stale_attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");

        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #2, streak -> 2 (a NEW attempt/snapshot)
        let current_offset = op.current_cq_offset_hz;
        let current_streak = op.cq_no_response_streak;

        // A late failure report for the OLD (CQ #1) attempt arrives now.
        op.restore_cq_state_for_attempt(stale_attempt_id);

        assert_eq!(
            op.cq_no_response_streak,
            current_streak - 1,
            "a stale, non-switching attempt_id must decrement the streak by exactly the one \
             increment it contributed, not leave it uncorrected"
        );
        assert_eq!(
            op.current_cq_offset_hz, current_offset,
            "a non-switching attempt never touched the offset, so compensating it needs no \
             offset correction"
        );
    }

    #[test]
    fn restore_cq_state_for_attempt_does_not_bounded_compensate_across_a_directed_reply_reset() {
        // PAN-38 round 3 (Codex): the bounded-compensation "-1" above assumes
        // the streak is purely additive across the gap between the stale
        // attempt's snapshot and the newer one that superseded it. A genuine
        // directed reply arriving in between breaks that assumption -- it
        // resets the streak to 0 independent of either attempt, so CQ #2's
        // "streak -> 1" reflects its OWN single increment off a fresh
        // baseline, not "CQ #1's contribution + CQ #2's contribution". A
        // blind `-1` would wrongly eat into CQ #2's own genuine increment
        // instead of the (already irrelevant) stale CQ #1 contribution the
        // reply already wiped out.
        let mut op = primed_operator(2, 5, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1, streak -> 1
        let stale_attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");

        // A genuine directed reply arrives before CQ #2 -- resets the streak
        // to 0 independent of either self-CQ attempt.
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("K9ZZ".into()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "W1ABC K9ZZ -05".into(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }],
            &NullDxEvaluator,
        );

        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #2 (post-reset), streak -> 1 again
        let current_streak = op.cq_no_response_streak;
        assert_eq!(
            current_streak, 1,
            "CQ #2 fired off the reply-reset baseline, so the streak is exactly its own \
             one increment"
        );

        // A late failure report for the OLD (CQ #1) attempt arrives now --
        // its speculative "+1" was already erased by the directed reply, so
        // there is nothing left to compensate.
        op.restore_cq_state_for_attempt(stale_attempt_id);

        assert_eq!(
            op.cq_no_response_streak, current_streak,
            "a directed-reply reset in between must block the bounded-compensation -1 -- \
             applying it anyway would wrongly eat into CQ #2's own genuine increment"
        );
    }

    #[test]
    fn restore_cq_state_for_attempt_does_not_auto_correct_a_stale_switching_attempt_id() {
        // PAN-38 round 1: the flip side of the test above — when the STALE
        // attempt is the one that performed a frequency switch, undoing it
        // would mean restoring an absolute pre-attempt baseline, unsafe once
        // superseded (risks discarding a legitimate later switch). This case
        // stays a no-op (with a warning logged, not asserted here), same as
        // before this round — just now for a narrower, honestly-documented
        // reason instead of blanket "always a no-op."
        //
        // `switch_after: 0` so the very FIRST self-CQ already satisfies
        // `streak (0) >= switch_after` and triggers a switch immediately —
        // simplest way to get a switching CQ #1 without needing several
        // cycles to build up a real streak first.
        let mut op = primed_operator(2, 0, true);
        for _ in 0..8 {
            op.feed_decoded_messages(&[], &NullDxEvaluator);
        }
        let even_ts: i64 = 0;
        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #1: streak (0) >= switch_after (0) -> switches, streak -> 1
        let stale_attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");
        assert!(
            op.last_cq_snapshot.is_some_and(|s| s.did_switch),
            "precondition: CQ #1 must have performed the switch this test exercises"
        );

        op.decide_at(even_ts); // idle
        op.decide_at(even_ts); // CQ #2, streak -> 2 (a NEW attempt/snapshot, no switch)
        let current_offset = op.current_cq_offset_hz;
        let current_streak = op.cq_no_response_streak;

        op.restore_cq_state_for_attempt(stale_attempt_id);

        assert_eq!(
            op.cq_no_response_streak, current_streak,
            "a stale SWITCHING attempt_id must not be auto-corrected — risks discarding a \
             legitimate later switch"
        );
        assert_eq!(
            op.current_cq_offset_hz, current_offset,
            "a stale SWITCHING attempt_id must not be auto-corrected — risks discarding a \
             legitimate later switch"
        );
    }

    #[test]
    fn hold_mode_invalidates_sticky_offset_even_without_a_cq_firing() {
        // Codex review (PR #276, round 6): the round-5 fix only invalidated
        // current_cq_offset_hz when a Hold-mode self-CQ actually ran. If the
        // operator toggles Auto -> Hold -> Auto between CQ opportunities (or
        // simply never goes idle enough to CQ while in Hold), no CQ ever
        // fires — the invalidation must still happen every cycle, not only
        // as a side effect of the self-CQ branch.
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        // High idle threshold: no self-CQ will ever fire during this test.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 1000;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode.clone());
        let stale_auto_offset = 1500.0;
        op.current_cq_offset_hz = Some(stale_auto_offset);

        mode.store(
            pancetta_core::TxFreqMode::Hold.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // no CQ fires (idle threshold is 1000)

        assert!(
            op.current_cq_offset_hz.is_none(),
            "current_cq_offset_hz must be invalidated on observing Hold mode, \
             even when no self-CQ ran this cycle"
        );
    }

    #[test]
    fn generation_bump_invalidates_sticky_offset_even_when_mode_reads_auto_both_times() {
        // PAN-39: an Auto -> Hold -> Auto round trip that completes entirely
        // between two `decide_at` polling cycles is invisible to a direct
        // read of the mode atomic — by the time the next cycle polls, the
        // mode already reads Auto again, same as before the round trip.
        // Simulate that "invisible" transition by bumping the generation
        // counter without ever setting the mode atomic to Hold — decide_at
        // must still discard the stale sticky offset.
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 1000; // no self-CQ fires during this test
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode.clone());
        op.set_tx_freq_mode_generation_source(generation.clone());

        let stale_auto_offset = 1500.0;
        op.current_cq_offset_hz = Some(stale_auto_offset);

        let even_ts: i64 = 0;
        op.decide_at(even_ts); // establishes the operator's baseline generation
        assert_eq!(
            op.current_cq_offset_hz,
            Some(stale_auto_offset),
            "no transition observed yet -- sticky offset must survive"
        );

        // Simulate a Hold entry+exit that happened entirely between polls:
        // the mode atomic is back to Auto, but the generation moved.
        generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            mode.load(std::sync::atomic::Ordering::Relaxed),
            pancetta_core::TxFreqMode::Auto.as_u8(),
            "mode reads Auto at poll time, exactly like the invisible-Hold scenario"
        );

        op.decide_at(even_ts);

        assert!(
            op.current_cq_offset_hz.is_none(),
            "a generation bump the mode-atomic read can't see must still \
             invalidate the sticky offset"
        );
    }

    #[test]
    fn refresh_tx_freq_offset_invalidation_catches_a_bump_between_two_calls_in_one_cycle() {
        // PAN-38 round 4 (Codex): `decide_at` calls
        // `refresh_tx_freq_offset_invalidation` once at the top AND again
        // immediately before the routine self-CQ path actually consumes
        // `current_cq_offset_hz`, several hundred lines later in the same
        // call -- a concurrent Hold->Auto round trip landing in that
        // in-between window would otherwise go unnoticed by the top-level
        // check alone, letting a self-CQ reuse a stale offset. Directly
        // exercise the two-calls-with-a-bump-in-between pattern (the
        // narrowest reproduction of the actual intra-cycle race, since a
        // synchronous unit test can't literally pause mid-`decide_at`).
        let mode = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        ));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(mode);
        op.set_tx_freq_mode_generation_source(generation.clone());

        let sticky_offset = 1700.0;
        op.current_cq_offset_hz = Some(sticky_offset);
        op.refresh_tx_freq_offset_invalidation(); // establishes the baseline generation
        assert_eq!(
            op.current_cq_offset_hz,
            Some(sticky_offset),
            "no transition observed yet -- sticky offset must survive the first call"
        );

        // A Hold entry+exit happens entirely between the two calls -- the
        // exact window the top-of-decide_at check alone can't see.
        generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        op.refresh_tx_freq_offset_invalidation(); // the pre-consumption revalidation
        assert!(
            op.current_cq_offset_hz.is_none(),
            "a generation bump landing between the two calls in one decide_at cycle must \
             still be caught before the offset is actually consumed"
        );
    }

    #[test]
    fn test_decision_engine_idle_to_cq() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.cq_after_idle_cycles = 3;
        // Set a high listen interval so we always transmit for testing.
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        // Use a fixed timestamp that falls on an Even slot (slot 0, t=0).
        let even_ts: i64 = 0; // unix epoch = slot 0 = Even

        // Run 3 idle cycles (no CQs, no QSOs).
        for _ in 0..2 {
            let actions = op.decide_at(even_ts);
            // Should either listen or produce a status.
            assert!(actions
                .iter()
                .any(|a| matches!(a, OperatorAction::Listen | OperatorAction::StatusUpdate(_))));
        }

        // 3rd cycle should trigger CQ.
        let actions = op.decide_at(even_ts);
        let has_transmit = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.starts_with("CQ")
            } else {
                false
            }
        });
        assert!(has_transmit, "Expected CQ after idle cycles");
    }

    #[test]
    fn test_decision_engine_respond_to_cq() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        // Feed a good CQ.
        let messages = vec![DecodedMessageInfo {
            callsign: Some("K9ZZ".into()),
            frequency_hz: 1500.0,
            snr: -5,
            message_text: "CQ K9ZZ EM48".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        let evaluator = NullDxEvaluator; // returns 0.5, above our 0.3 threshold
        op.feed_decoded_messages(&messages, &evaluator);

        // Use a fixed Even timestamp.
        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        let has_response = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.contains("W1ABC")
            } else {
                false
            }
        });
        assert!(has_response, "Expected response to CQ");
    }

    #[test]
    fn test_decision_engine_skips_cq_blocked_by_fp_filter() {
        // Phase-5 hardening #1: with an FP filter installed and the CQ
        // sender absent from the trust set, the responder must NOT
        // transmit. Mirrors the 2026-05-30 audit scenario where
        // OSD-fabricated calls (`R44XYB`, `OR1QRD`) flowed through to
        // the autonomous TX path.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        // Build a strict-mode filter that knows only "K1KNOWN".
        let mut filter = crate::callsign_continuity::CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1KNOWN"]);
        op.set_fp_filter(Some(std::sync::Arc::new(filter)));

        // Feed an unknown-callsign CQ with a passing score.
        let messages = vec![DecodedMessageInfo {
            callsign: Some("R44XYB".into()),
            frequency_hz: 1500.0,
            snr: -5,
            message_text: "CQ R44XYB FN42".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        op.feed_decoded_messages(&messages, &NullDxEvaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        let has_response = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.contains("R44XYB")
            } else {
                false
            }
        });
        assert!(
            !has_response,
            "Expected FP filter to block response to OSD-fabricated callsign"
        );
    }

    #[test]
    fn test_decision_engine_responds_when_fp_filter_accepts() {
        // Phase-5 hardening #1: with an FP filter installed and the CQ
        // sender present in the trust set, the responder behaves as
        // before — proves the gate isn't broken for legitimate decodes.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        let mut filter = crate::callsign_continuity::CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K9ZZ"]);
        op.set_fp_filter(Some(std::sync::Arc::new(filter)));

        let messages = vec![DecodedMessageInfo {
            callsign: Some("K9ZZ".into()),
            frequency_hz: 1500.0,
            snr: -5,
            message_text: "CQ K9ZZ EM48".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        op.feed_decoded_messages(&messages, &NullDxEvaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        let has_response = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.contains("K9ZZ")
            } else {
                false
            }
        });
        assert!(
            has_response,
            "Expected responder to TX when CQ sender passes FP filter"
        );
    }

    #[test]
    fn test_content_score_blocks_low_score_cq_at_autonomous_tx() {
        // hb-103 (Batch 32): even when the FP filter accepts the callsign,
        // a sufficiently low content score blocks autonomous TX.
        // SHIP_CONSERVATIVE = +0.35; very-low confidence + late dt + bad
        // SNR + no trust-bonus → score well below threshold.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        let mut filter = crate::callsign_continuity::CallsignContinuityFilter::new(100);
        // Trust K9ZZ so hb-062 accepts; the content score still gates.
        filter.extend_from_iter(["K9ZZ"]);
        op.set_fp_filter(Some(std::sync::Arc::new(filter)));

        // confidence=0.1 + dt=12s + snr=-15 + 1 (any) - 0.1*12 - 0.05*15
        // = 1 + 0.1 - 1.2 - 0.75 = -0.85, below SHIP_CONSERVATIVE +0.35.
        let messages = vec![DecodedMessageInfo {
            callsign: Some("K9ZZ".into()),
            frequency_hz: 1500.0,
            snr: -15,
            message_text: "CQ K9ZZ EM48".into(),
            slot_parity: None,
            confidence: Some(0.1),
            time_offset_s: Some(12.0),
            decode_origin: None,
        }];
        op.feed_decoded_messages(&messages, &NullDxEvaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        let has_response = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.contains("K9ZZ")
            } else {
                false
            }
        });
        assert!(
            !has_response,
            "Expected hb-103 content-score gate to block low-score CQ even when callsign is trusted"
        );
    }

    #[test]
    fn test_v3_gate_origin_penalty_rejects_recovery_pass_cq() {
        // hb-247 (Batch 81): the v3 gate subtracts decode_origin/6.
        // Build a CQ whose v1-equivalent score sits between
        // SHIP_CONSERVATIVE and SHIP_CONSERVATIVE + 1 so that an
        // aggressive-recovery origin (6 → penalty 1.0) pushes it below
        // the threshold, while the same decode at origin 0 / None passes.
        // Trusted callsign (mirrors the existing gate fixtures):
        // 1 (any-trust) + 0.5 (conf) - 0.1*4 (dt) + 0.05*-5 (snr)
        // = 0.85 → above +0.35; minus 6/6 = -0.15 → below.
        let make_op = || {
            let mut config = AutonomousConfig::default();
            config.enabled = true;
            config.slot_parity = SlotParityConfig::Even;
            config.min_dx_score = 0.0;
            config.listen_cycle.initial_interval = 100;
            let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
            let mut filter = crate::callsign_continuity::CallsignContinuityFilter::new(100);
            filter.extend_from_iter(["K9ZZ"]);
            op.set_fp_filter(Some(std::sync::Arc::new(filter)));
            op
        };
        let msg = |origin: Option<u8>| {
            vec![DecodedMessageInfo {
                callsign: Some("K9ZZ".into()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "CQ K9ZZ EM48".into(),
                slot_parity: None,
                confidence: Some(0.5),
                time_offset_s: Some(4.0),
                decode_origin: origin,
            }]
        };
        let responded = |actions: &[OperatorAction]| {
            actions.iter().any(|a| {
                matches!(a, OperatorAction::Transmit { message_text, .. } if message_text.contains("K9ZZ"))
            })
        };

        let mut op = make_op();
        op.feed_decoded_messages(&msg(None), &NullDxEvaluator);
        assert!(
            responded(&op.decide_at(0)),
            "origin=None must behave like the old v1 gate (score 0.85 > 0.35)"
        );

        let mut op = make_op();
        op.feed_decoded_messages(&msg(Some(0)), &NullDxEvaluator);
        assert!(
            responded(&op.decide_at(0)),
            "origin=0 (primary pass) must be byte-identical to v1 gate"
        );

        let mut op = make_op();
        op.feed_decoded_messages(&msg(Some(6)), &NullDxEvaluator);
        assert!(
            !responded(&op.decide_at(0)),
            "origin=6 penalty (-1.0) must push 0.85 below SHIP_CONSERVATIVE"
        );
    }

    #[test]
    fn test_content_score_permits_high_score_cq_at_autonomous_tx() {
        // hb-103 (Batch 32): when the content score clears SHIP_CONSERVATIVE
        // (+0.35), the responder TX's. Single-callsign CQ with high
        // confidence + low dt + trusted callsign + decent SNR comfortably
        // clears the conservative threshold.
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        let mut filter = crate::callsign_continuity::CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K9ZZ"]);
        op.set_fp_filter(Some(std::sync::Arc::new(filter)));

        // 1 (any) + 0.95 (conf) - 0.1*1 (dt) + 0.05*-5 (snr)
        // = 1 + 0.95 - 0.1 - 0.25 = 1.60, well above SHIP_CONSERVATIVE +0.35.
        let messages = vec![DecodedMessageInfo {
            callsign: Some("K9ZZ".into()),
            frequency_hz: 1500.0,
            snr: -5,
            message_text: "CQ K9ZZ EM48".into(),
            slot_parity: None,
            confidence: Some(0.95),
            time_offset_s: Some(1.0),
            decode_origin: None,
        }];
        op.feed_decoded_messages(&messages, &NullDxEvaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);
        let has_response = actions.iter().any(|a| {
            if let OperatorAction::Transmit { message_text, .. } = a {
                message_text.contains("K9ZZ")
            } else {
                false
            }
        });
        assert!(
            has_response,
            "Expected responder to TX when content score clears SHIP_CONSERVATIVE"
        );
    }

    #[test]
    fn test_band_strategy_hop() {
        let config = BandHoppingConfig {
            enabled: true,
            hop_threshold: 3,
            bands: vec![
                BandEntry {
                    dial_frequency: 14_074_000,
                    band_name: "20m".into(),
                    priority: 1,
                },
                BandEntry {
                    dial_frequency: 7_074_000,
                    band_name: "40m".into(),
                    priority: 2,
                },
            ],
        };

        let mut strategy = BandStrategy::new(config);
        assert_eq!(strategy.current_band_name(), "20m");

        // 3 zero-activity cycles should trigger a hop.
        strategy.record_activity(0);
        strategy.record_activity(0);
        strategy.record_activity(0);
        let hop = strategy.should_hop();
        assert_eq!(hop, Some(7_074_000));
        assert_eq!(strategy.current_band_name(), "40m");
    }

    #[test]
    fn test_pause_resume() {
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), None);

        assert!(!op.is_paused());
        op.pause();
        assert!(op.is_paused());
        assert_eq!(*op.state(), OperatingState::Paused);

        let actions = op.decide();
        // When paused, should only get status updates.
        assert!(actions
            .iter()
            .all(|a| matches!(a, OperatorAction::StatusUpdate(_))));

        op.resume();
        assert!(!op.is_paused());
    }

    // --- Frequency allocator tests ---

    #[test]
    fn test_frequency_allocator_basic() {
        let alloc = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        assert!(alloc.is_clear_of_own(1500.0));
    }

    #[test]
    fn test_frequency_allocator_own_separation() {
        let mut alloc = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        alloc.register_qso_frequency("qso1", 1500.0);

        // Too close
        assert!(!alloc.is_clear_of_own(1550.0));
        // Far enough
        assert!(alloc.is_clear_of_own(1600.0));
        // Exact boundary
        assert!(alloc.is_clear_of_own(1575.0));

        alloc.release_qso_frequency("qso1");
        assert!(alloc.is_clear_of_own(1550.0));
    }

    /// FQ-F3: `set_own_frequencies` must wholesale-replace the registry —
    /// entries from a previous call that are absent from a new snapshot
    /// must be gone, not merged/accumulated.
    #[test]
    fn test_frequency_allocator_set_own_frequencies_bulk_replace() {
        let mut alloc = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        alloc.register_qso_frequency("qso1", 1500.0);
        assert!(!alloc.is_clear_of_own(1550.0));

        // A fresh snapshot that does NOT include qso1 must fully replace
        // the old state — qso1's entry must be gone, not merged.
        let mut snapshot = HashMap::new();
        snapshot.insert("qso2".to_string(), 2000.0);
        alloc.set_own_frequencies(snapshot);

        assert!(
            alloc.is_clear_of_own(1550.0),
            "stale qso1 entry must be gone after a bulk replace that omits it"
        );
        assert!(
            !alloc.is_clear_of_own(2050.0),
            "qso2's entry must be present"
        );

        // Replacing with an empty map clears everything.
        alloc.set_own_frequencies(HashMap::new());
        assert!(alloc.is_clear_of_own(2050.0));
    }

    /// FQ-F3: syncing the own-frequency registry via `set_own_frequencies`
    /// must actually change `allocate_smart_frequency`'s real output —
    /// proving the registry isn't just plumbed in but load-bearing: a
    /// candidate that collides with a registered own-frequency must be
    /// avoided in favor of a different offset once registered.
    #[test]
    fn set_own_frequencies_changes_allocate_smart_frequency_output() {
        let config = AutonomousConfig {
            enabled: true,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0; 128],
            freq_min_hz: 0.0,
            freq_max_hz: 3000.0,
        });

        let baseline = op.allocate_smart_frequency(None, None, None);

        // Register (via bulk replace) an own-frequency exactly at the
        // baseline pick — criterion #7's -50 penalty should knock it out
        // of contention, so the new best pick must differ.
        let mut frequencies = HashMap::new();
        frequencies.insert("qso-1".to_string(), baseline);
        op.frequency_allocator_mut()
            .set_own_frequencies(frequencies);

        let after = op.allocate_smart_frequency(None, None, None);
        assert_ne!(
            after, baseline,
            "an own-frequency collision at the baseline pick must move the \
             allocator's choice away from it"
        );
    }

    /// FQ-F6: Hold-mode must prefer the LIVE parked-offset atomic (set via
    /// the TUI's `o` modal) over the static `config.tx_offset_hz` when a
    /// non-zero value is actually parked.
    #[test]
    fn hold_mode_prefers_live_parked_offset_over_config() {
        let config = AutonomousConfig {
            tx_offset_hz: 1500.0,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        // Explicit Hold mode (also the default).
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Hold.as_u8(),
        )));

        let hold_hz = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        op.set_tx_offset_hold_source(hold_hz.clone());

        // Nothing parked yet (atomic == 0): must fall back to the static
        // config value, byte-identical to pre-fix behavior.
        assert_eq!(
            op.allocate_smart_frequency(None, None, None),
            1500.0,
            "unset parked-offset atomic must fall back to config.tx_offset_hz"
        );

        // Operator parks a live offset via the `o` modal (simulated store).
        hold_hz.store(2137, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            op.allocate_smart_frequency(None, None, None),
            2137.0,
            "a non-zero parked offset must win over the static config value"
        );

        // Operator clears the park (0 = unset again): must revert to config.
        hold_hz.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            op.allocate_smart_frequency(None, None, None),
            1500.0,
            "clearing the park (0) must revert to config.tx_offset_hz"
        );
    }

    /// FQ-F6 regression: an `AutonomousOperator` that never had
    /// `set_tx_offset_hold_source` called (e.g. a bare unit test or any
    /// caller that predates this fix) must behave exactly as before —
    /// Hold mode returns the static config value.
    #[test]
    fn hold_mode_without_offset_source_wired_uses_config_value() {
        let config = AutonomousConfig {
            tx_offset_hz: 1234.0,
            ..AutonomousConfig::default()
        };
        let op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        // Default mode is Hold; no `set_tx_offset_hold_source` call at all.
        assert_eq!(op.allocate_smart_frequency(None, None, None), 1234.0);
    }

    /// FQ-F8: `allocate_smart_frequency`'s `target_parity` parameter must
    /// actually reach `rank_candidates_with_parity`, correctly mapped
    /// (`SlotParity::Even -> TimeSlot::First`, `Odd -> TimeSlot::Second`) —
    /// this is the wiring gap between the parity-aware scoring fix and the
    /// one production caller (the pounce path) that has a real DX parity to
    /// supply. Rather than trying to force the overall winning frequency to
    /// flip (fragile — the DX-proximity term can dominate a small, localized
    /// occupancy difference and mask a mapping bug), this asserts a direct
    /// consistency property: calling `allocate_smart_frequency` with a given
    /// `SlotParity` must produce EXACTLY the same result as calling
    /// `rank_candidates_with_parity` directly with the correspondingly
    /// mapped `TimeSlot` — proving the mapping and the plumbing are both
    /// correct, not just that the parameter is silently accepted and ignored
    /// (which None/None already covers via the `hold_mode_*` tests above).
    #[test]
    fn allocate_smart_frequency_honors_target_parity_end_to_end() {
        use pancetta_core::slot::SlotParity;

        let config = AutonomousConfig {
            enabled: true,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0; 128],
            freq_min_hz: 0.0,
            freq_max_hz: 3000.0,
        });
        op.decode_history.push_cycle(vec![
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            },
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            },
        ]);

        let own_freqs: Vec<f64> = op
            .frequency_allocator
            .own_frequencies()
            .values()
            .copied()
            .collect();
        let spectral = op.spectral_snapshot.as_ref().unwrap();

        for (slot_parity, expected_time_slot) in [
            (SlotParity::Even, TimeSlot::First),
            (SlotParity::Odd, TimeSlot::Second),
        ] {
            let via_operator = op.allocate_smart_frequency(Some(1500.0), Some(slot_parity), None);
            let via_direct_ranking = op
                .smart_allocator
                .rank_candidates_with_parity(
                    spectral,
                    &op.decode_history,
                    &own_freqs,
                    Some(1500.0),
                    Some(expected_time_slot),
                )
                .first()
                .unwrap()
                .offset_hz;

            assert_eq!(
                via_operator, via_direct_ranking,
                "allocate_smart_frequency(Some(1500.0), Some({slot_parity:?})) must match \
                 rank_candidates_with_parity's own top pick for the mapped TimeSlot \
                 ({expected_time_slot:?}) — a dropped/mismapped target_parity would diverge"
            );
        }
    }

    #[test]
    fn test_frequency_allocator_cq_avoids_own() {
        let mut alloc = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        alloc.register_qso_frequency("qso1", 1500.0);

        let freq = alloc.allocate_cq_frequency();
        // Should be at least 75 Hz away from 1500
        assert!(
            (freq - 1500.0).abs() >= 75.0,
            "CQ freq {:.0} too close to 1500",
            freq
        );
    }

    // --- Multi-QSO decision tests ---

    #[test]
    fn test_multi_qso_emit_multiple_transmits() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.max_concurrent_qsos = 3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_active_qso_count(2);

        // Feed two sequencer messages at different frequencies
        op.add_pending_sequencer_message("K9ZZ W1ABC -12".into(), 1500.0, Some("qso1".into()));
        op.add_pending_sequencer_message("VE3ABC W1ABC R-15".into(), 1700.0, Some("qso2".into()));

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        assert_eq!(
            tx_actions.len(),
            2,
            "Expected 2 Transmit actions, got {}",
            tx_actions.len()
        );
    }

    #[test]
    fn test_multi_qso_respects_max_concurrent() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.max_concurrent_qsos = 2;
        config.min_dx_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_active_qso_count(2);

        // Two active QSOs with pending messages
        op.add_pending_sequencer_message("K9ZZ W1ABC -12".into(), 1500.0, Some("qso1".into()));
        op.add_pending_sequencer_message("VE3ABC W1ABC R-15".into(), 1700.0, Some("qso2".into()));

        // Feed a CQ too
        let messages = vec![DecodedMessageInfo {
            callsign: Some("JA1ABC".into()),
            frequency_hz: 2000.0,
            snr: -5,
            message_text: "CQ JA1ABC PM95".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        let evaluator = NullDxEvaluator;
        op.feed_decoded_messages(&messages, &evaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should emit 2 (existing QSOs) but NOT respond to CQ (at max)
        assert_eq!(
            tx_actions.len(),
            2,
            "Expected 2 Transmit actions (at max), got {}",
            tx_actions.len()
        );
    }

    #[test]
    fn test_multi_qso_adds_new_when_capacity() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = SlotParityConfig::Even;
        config.max_concurrent_qsos = 3;
        config.min_dx_score = 0.3;
        config.min_multi_slot_score = 0.3; // Lower threshold so NullDxEvaluator (0.5) passes
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_active_qso_count(1);

        // One active QSO
        op.add_pending_sequencer_message("K9ZZ W1ABC -12".into(), 1500.0, Some("qso1".into()));

        // Feed a CQ at a different frequency
        let messages = vec![DecodedMessageInfo {
            callsign: Some("JA1ABC".into()),
            frequency_hz: 2000.0,
            snr: -5,
            message_text: "CQ JA1ABC PM95".into(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }];
        let evaluator = NullDxEvaluator;
        op.feed_decoded_messages(&messages, &evaluator);

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should emit 2: one sequencer message + one CQ response
        assert_eq!(
            tx_actions.len(),
            2,
            "Expected 2 Transmit actions (1 QSO + 1 new CQ response), got {}",
            tx_actions.len()
        );
    }

    /// Test helper: evaluator that returns a fixed score
    struct HighScoreEvaluator(f64);
    impl DxEvaluator for HighScoreEvaluator {
        fn evaluate_cq(&self, _: &str, _: Option<&str>, _: i8, _: f64) -> f64 {
            self.0
        }
    }

    #[test]
    fn null_dx_evaluator_tiered_defaults_to_none() {
        let evaluator = NullDxEvaluator;
        assert_eq!(
            evaluator.evaluate_cq_tiered("W1ABC", None, -10, 14_074_000.0),
            None,
            "evaluators that don't implement tiered scoring must default to None"
        );
    }

    /// Test helper: evaluator whose tier is controllable per-callsign, default
    /// score irrelevant to these tests.
    struct TieredTestEvaluator {
        tier_for: std::collections::HashMap<String, PriorityTier>,
    }
    impl TieredTestEvaluator {
        fn new() -> Self {
            Self {
                tier_for: std::collections::HashMap::new(),
            }
        }
        fn with_tier(mut self, callsign: &str, tier: PriorityTier) -> Self {
            self.tier_for.insert(callsign.to_string(), tier);
            self
        }
    }
    impl DxEvaluator for TieredTestEvaluator {
        fn evaluate_cq(&self, _: &str, _: Option<&str>, _: i8, _: f64) -> f64 {
            0.5
        }
        fn evaluate_cq_tiered(
            &self,
            callsign: &str,
            _: Option<&str>,
            _: i8,
            _: f64,
        ) -> Option<TieredScore> {
            self.tier_for.get(callsign).map(|&tier| TieredScore {
                tier,
                secondary: 0.5,
            })
        }
    }

    fn cq_message(callsign: &str, grid: &str) -> DecodedMessageInfo {
        DecodedMessageInfo {
            callsign: Some(callsign.to_string()),
            frequency_hz: 1500.0,
            snr: -10,
            message_text: format!("CQ {callsign} {grid}"),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }
    }

    #[test]
    fn watchlist_gains_per_band_dxcc_new_cq_even_when_at_tx_capacity() {
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));
        // Simulate being at full TX capacity — feed_decoded_messages_at must
        // still populate the watchlist regardless of decide_at()'s capacity gate.
        op.set_active_qso_count(999);

        let evaluator =
            TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
        op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, Utc::now());

        assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn watchlist_ignores_standard_tier_cqs() {
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        let evaluator = TieredTestEvaluator::new().with_tier("W2XYZ", PriorityTier::Standard);
        op.feed_decoded_messages_at(&[cq_message("W2XYZ", "FN42")], &evaluator, Utc::now());

        assert!(
            op.watchlist_callsigns().is_empty(),
            "Standard-tier CQs must never enter the watchlist"
        );
    }

    #[test]
    fn watchlist_ignores_untiered_evaluators() {
        // NullDxEvaluator (and any evaluator that doesn't implement tiered
        // scoring) must never populate the watchlist — `evaluate_cq_tiered`
        // defaults to None, which this feed loop must skip.
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        op.feed_decoded_messages_at(
            &[cq_message("JA1ABC", "PM95")],
            &NullDxEvaluator,
            Utc::now(),
        );

        assert!(op.watchlist_callsigns().is_empty());
    }

    #[test]
    fn watchlist_entry_expires_after_ttl_with_no_rehear() {
        let mut config = AutonomousConfig::default();
        config.watchlist_ttl_secs = 150;
        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        let t0 = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
        let evaluator =
            TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
        op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, t0);
        assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);

        // Feed again, 151s later, with no re-hear of JA1ABC at all.
        let t1 = t0 + ChronoDuration::seconds(151);
        op.feed_decoded_messages_at(&[], &evaluator, t1);

        assert!(
            op.watchlist_callsigns().is_empty(),
            "entry must be pruned once its TTL elapses with no re-hear"
        );
    }

    #[test]
    fn watchlist_entry_survives_within_ttl_with_no_rehear() {
        let mut config = AutonomousConfig::default();
        config.watchlist_ttl_secs = 150;
        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        let t0 = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
        let evaluator =
            TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
        op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, t0);

        // Feed again, only 30s later, with no re-hear — must still be present.
        let t1 = t0 + ChronoDuration::seconds(30);
        op.feed_decoded_messages_at(&[], &evaluator, t1);

        assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn test_multi_slot_opens_for_high_score() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;
        config.min_multi_slot_score = 0.5;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        // Simulate one active QSO
        op.set_active_qso_count(1);
        op.add_pending_sequencer_message(
            "K1XYZ W1ABC -10".to_string(),
            1000.0,
            Some("qso-1".to_string()),
        );
        op.frequency_allocator_mut()
            .register_qso_frequency("qso-1", 1000.0);

        // Feed a high-scoring CQ
        let evaluator = HighScoreEvaluator(0.8);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("3Y0J".to_string()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "CQ 3Y0J JD15".to_string(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }],
            &evaluator,
        );

        let even_ts: i64 = 0; // Even slot
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should have 2 transmissions: sequencer message + new CQ response
        assert_eq!(
            tx_actions.len(),
            2,
            "Expected 2 TX actions, got {:?}",
            tx_actions
        );
    }

    #[test]
    fn test_multi_slot_blocked_by_low_score() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;
        config.min_multi_slot_score = 0.9; // Very high threshold
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        op.set_active_qso_count(1);
        op.add_pending_sequencer_message(
            "K1XYZ W1ABC -10".to_string(),
            1000.0,
            Some("qso-1".to_string()),
        );
        op.frequency_allocator_mut()
            .register_qso_frequency("qso-1", 1000.0);

        // Feed a moderate-scoring CQ (below threshold)
        let evaluator = HighScoreEvaluator(0.6);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("VE3XYZ".to_string()),
                frequency_hz: 1500.0,
                snr: -10,
                message_text: "CQ VE3XYZ FN03".to_string(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }],
            &evaluator,
        );

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should only have 1 transmission (existing QSO, not the new CQ)
        assert_eq!(
            tx_actions.len(),
            1,
            "Expected 1 TX action, got {:?}",
            tx_actions
        );
    }

    #[test]
    fn test_max_concurrent_qsos_respected() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;
        config.min_multi_slot_score = 0.3;
        config.listen_cycle.initial_interval = 100;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

        // Already at max QSOs
        op.set_active_qso_count(2);
        op.add_pending_sequencer_message(
            "K1A W1ABC -10".to_string(),
            1000.0,
            Some("q1".to_string()),
        );
        op.add_pending_sequencer_message(
            "K2B W1ABC -12".to_string(),
            1200.0,
            Some("q2".to_string()),
        );

        let evaluator = HighScoreEvaluator(0.95);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("3Y0J".to_string()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "CQ 3Y0J JD15".to_string(),
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            }],
            &evaluator,
        );

        let even_ts: i64 = 0;
        let actions = op.decide_at(even_ts);

        let tx_count = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .count();

        // Should NOT add a third QSO
        assert_eq!(tx_count, 2, "Should not exceed max_concurrent_qsos");
        assert_eq!(
            op.take_skip_log(),
            vec![CqSkipRecord {
                callsign: None,
                reason: SkipReason::AtCapacity { active: 2, cap: 2 },
            }]
        );
        assert!(op.take_skip_log().is_empty(), "taking the log drains it");
    }

    #[test]
    fn placement_snapshot_ranks_and_bins() {
        // Build the operator fixture the same way existing autonomous tests
        // do, then feed spectral + one busy decode and ask for a snapshot.
        let config = AutonomousConfig {
            enabled: true,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        // No spectral snapshot yet -> None.
        assert!(op.placement_snapshot(10).is_none());

        op.update_spectral(SpectralSnapshot {
            power_bins: vec![0.0; 128],
            freq_min_hz: 200.0,
            freq_max_hz: 3000.0,
        });
        op.decode_history_mut_for_test()
            .push_cycle(vec![DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            }]);

        let snap = op.placement_snapshot(10).unwrap();
        assert_eq!(snap.slices.len(), 10);
        assert!(
            snap.slices.windows(2).all(|w| w[0].score >= w[1].score),
            "sorted by score desc"
        );
        let bin_1500 = ((1500.0 - snap.range.0) / snap.bin_hz) as usize;
        assert_eq!(
            snap.openness[bin_1500], 1,
            "busy in First -> second-only-clear"
        );
    }

    /// Single-scorer invariant: the real CQ-frequency decision
    /// (`allocate_smart_frequency`) and the TX-placement instrument
    /// (`placement_snapshot`) must agree on the top-ranked candidate when
    /// the CQ-mode live-spot rarity nudge is in play.
    ///
    /// With a flat spectral snapshot and empty decode history, every
    /// candidate's natural score differs only by the center-bias term,
    /// which — at the default `step_hz=25.0` / `range=(200,2800)` — steps
    /// by ~0.192 per 25 Hz. A single rarity=1.0 live spot contributes a
    /// flat +0.2 to every candidate within 200 Hz of it, which exceeds
    /// that step. Placing the spot at 1720 Hz puts 1525 Hz (the natural
    /// runner-up, 195 Hz away) inside the boost window while leaving the
    /// natural #1 pick, 1500 Hz (220 Hz away, the exact center-bias peak),
    /// just outside it — so the boost provably flips the winner from 1500
    /// to 1525 Hz. If only one of the two code paths applied the boost,
    /// they would disagree on the winner here.
    #[test]
    fn placement_snapshot_agrees_with_real_cq_decision_under_live_spot_boost() {
        let config = AutonomousConfig {
            enabled: true,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));

        // Auto mode: the real decision path only consults the smart
        // allocator when the operator has released the TX offset.
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));

        op.update_spectral(SpectralSnapshot {
            power_bins: vec![],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        op.update_live_spots(&[(1720.0, 1.0)]);

        // Sanity: without the boost, the natural winner is the exact
        // center-bias peak.
        let unboosted = op
            .smart_allocator
            .rank_candidates(
                op.spectral_snapshot.as_ref().unwrap(),
                &op.decode_history,
                &[],
                None,
            )
            .first()
            .unwrap()
            .offset_hz;
        assert_eq!(unboosted, 1500.0, "sanity: unboosted winner is band center");

        // Real CQ-frequency decision path.
        let real_freq = op.allocate_smart_frequency(None, None, None);

        // TX-placement instrument.
        let snap = op.placement_snapshot(usize::MAX).unwrap();
        let instrument_top = &snap.slices[0];

        assert_eq!(
            real_freq, instrument_top.offset_hz,
            "placement instrument's top pick must match the real CQ-frequency decision"
        );
        assert_eq!(
            real_freq, 1525.0,
            "boost should flip the winner away from the unboosted 1500 Hz peak"
        );

        // Full-order agreement, not just the top pick: re-derive the real
        // path's boosted-and-sorted candidate list and compare offsets +
        // scores 1:1 against the instrument's (unboosted-candidate order
        // is a superset check since the instrument returns all of them
        // here via usize::MAX).
        let mut real_candidates = op.smart_allocator.rank_candidates(
            op.spectral_snapshot.as_ref().unwrap(),
            &op.decode_history,
            &[],
            None,
        );
        AutonomousOperator::apply_live_spot_rarity_boost(
            &mut real_candidates,
            &op.live_spot_frequencies,
        );
        assert_eq!(real_candidates.len(), snap.slices.len());
        for (a, b) in real_candidates.iter().zip(snap.slices.iter()) {
            assert_eq!(a.offset_hz, b.offset_hz);
            assert_eq!(a.score, b.score);
        }
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn build_test_operator(our_call: &str) -> AutonomousOperator {
        let config = AutonomousConfig {
            enabled: true,
            ..AutonomousConfig::default()
        };
        AutonomousOperator::new(config, our_call.into(), None)
    }

    #[test]
    fn skips_recent_cq_from_same_callsign() {
        let mut op = build_test_operator("K5ARH");
        let now = Utc::now();

        op.recently_responded_to.insert("AB1CD".to_string(), now);

        assert!(
            op.is_recently_responded_to("AB1CD", now + ChronoDuration::seconds(30)),
            "60s window not honored — should still be skipping"
        );
    }

    #[test]
    fn allows_cq_from_callsign_after_window() {
        let mut op = build_test_operator("K5ARH");
        let now = Utc::now();

        op.recently_responded_to.insert("AB1CD".to_string(), now);

        assert!(
            !op.is_recently_responded_to("AB1CD", now + ChronoDuration::seconds(70)),
            "after 60s, should accept again"
        );
    }

    #[test]
    fn mark_responded_to_prunes_stale() {
        let mut op = build_test_operator("K5ARH");
        let stale = Utc::now() - ChronoDuration::seconds(60 * 60); // 1 hour ago
        op.recently_responded_to.insert("STALE".to_string(), stale);
        let now = Utc::now();
        op.mark_responded_to("FRESH", now);
        // Stale should be pruned (older than 5 × 60s = 300s).
        assert!(!op.recently_responded_to.contains_key("STALE"));
        assert!(op.recently_responded_to.contains_key("FRESH"));
    }
}

#[cfg(test)]
mod dx_busy_tests {
    use super::*;
    use chrono::TimeZone;

    fn op_even(our: &str) -> AutonomousOperator {
        let config = AutonomousConfig {
            enabled: true,
            slot_parity: SlotParityConfig::Even,
            min_dx_score: 0.3,
            // Force CQ responses, never our own CQ, deterministic slot.
            listen_cycle: ListenCycleConfig {
                initial_interval: 100,
                ..ListenCycleConfig::default()
            },
            ..AutonomousConfig::default()
        };
        AutonomousOperator::new(config, our.into(), Some("FN42".into()))
    }

    fn dmi(text: &str, call: Option<&str>, freq: f64) -> DecodedMessageInfo {
        DecodedMessageInfo {
            callsign: call.map(|c| c.to_string()),
            frequency_hz: freq,
            snr: -5,
            message_text: text.to_string(),
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }
    }

    #[test]
    fn parser_detects_third_party_exchange() {
        // "<to> <from> <report>" with neither being us → both busy.
        let calls = third_party_exchange_callsigns("JA1ABC W1XYZ -12", "K5ARH");
        assert_eq!(calls, vec!["JA1ABC".to_string(), "W1XYZ".to_string()]);
        // RR73 / 73 also count.
        assert_eq!(
            third_party_exchange_callsigns("JA1ABC W1XYZ RR73", "K5ARH").len(),
            2
        );
        // A reply carrying a grid is NOT a committed exchange.
        assert!(third_party_exchange_callsigns("JA1ABC W1XYZ FN42", "K5ARH").is_empty());
        // CQ never counts.
        assert!(third_party_exchange_callsigns("CQ JA1ABC PM95", "K5ARH").is_empty());
        // If WE are a party, it is our own QSO, not a third party.
        assert!(third_party_exchange_callsigns("K5ARH JA1ABC -12", "K5ARH").is_empty());
        assert!(third_party_exchange_callsigns("JA1ABC K5ARH R-12", "K5ARH").is_empty());
    }

    #[test]
    fn busy_dx_response_is_suppressed_then_permitted_after_window() {
        let mut op = op_even("K5ARH");
        let evaluator = NullDxEvaluator; // 0.5 ≥ 0.3 threshold

        // A fixed even-parity virtual slot time. Unix epoch (slot 0) is
        // Even; `decide_at(0)` derives the same instant for its gates.
        let now = Utc.timestamp_opt(0, 0).single().expect("epoch is valid");

        // Slot A: JA1ABC is working W1XYZ (a third party).
        op.feed_decoded_messages_at(
            &[dmi("JA1ABC W1XYZ -12", Some("W1XYZ"), 1500.0)],
            &evaluator,
            now,
        );
        assert!(
            op.is_dx_busy("JA1ABC", now),
            "JA1ABC should be flagged busy after a third-party exchange"
        );

        // Slot B: JA1ABC briefly CQs again. Because it was just busy, the
        // autonomous operator must NOT respond.
        op.feed_decoded_messages_at(
            &[dmi("CQ JA1ABC PM95", Some("JA1ABC"), 1500.0)],
            &evaluator,
            now,
        );
        let actions = op.decide_at(0); // even slot → our TX slot, now=epoch
        let responded = actions.iter().any(|a| {
            matches!(a, OperatorAction::Transmit { message_text, .. } if message_text.contains("JA1ABC"))
        });
        assert!(!responded, "must suppress response to a busy DX");
        assert!(op.take_skip_log().iter().any(|record| {
            record.callsign.as_deref() == Some("JA1ABC")
                && matches!(record.reason, SkipReason::DxBusy { window_secs: 90 })
        }));
    }

    #[test]
    fn non_busy_dx_cq_is_answered() {
        let mut op = op_even("K5ARH");
        let evaluator = NullDxEvaluator;
        let now = Utc.timestamp_opt(0, 0).single().expect("epoch is valid");

        // JA1ABC simply CQs; never seen in an exchange → answerable.
        op.feed_decoded_messages_at(
            &[dmi("CQ JA1ABC PM95", Some("JA1ABC"), 1500.0)],
            &evaluator,
            now,
        );
        assert!(!op.is_dx_busy("JA1ABC", now));
        let actions = op.decide_at(0);
        let responded = actions.iter().any(|a| {
            matches!(a, OperatorAction::Transmit { message_text, .. } if message_text.contains("JA1ABC"))
        });
        assert!(responded, "a non-busy DX CQ should be answered");
        assert!(op.take_skip_log().is_empty());
    }

    #[test]
    fn skip_log_is_bounded() {
        let mut op = op_even("K5ARH");
        for i in 0..(MAX_SKIP_LOG_PER_SLOT * 2) {
            op.push_skip(CqSkipRecord {
                callsign: Some(format!("CALL{i}")),
                reason: SkipReason::FrequencyClash,
            });
        }
        assert_eq!(op.take_skip_log().len(), MAX_SKIP_LOG_PER_SLOT);
    }

    #[test]
    fn busy_flag_expires_after_window() {
        let mut op = op_even("K5ARH");
        let now = Utc::now();
        // Manually seed an old busy timestamp beyond the 90s window.
        let stale = now - ChronoDuration::seconds(120);
        op.recently_in_qso.insert("JA1ABC".to_string(), stale);
        assert!(
            !op.is_dx_busy("JA1ABC", now),
            "busy flag older than dx_busy_window_secs (90s) must expire"
        );
    }

    /// FQ-F2 (part 1): `feed_decoded_messages_at` must stamp each decoded
    /// message's `DecodeHistory` entry using THAT message's own carried
    /// `slot_parity`, not the wall clock at feed time. We prove this by
    /// picking a message parity that DISAGREES with the real wall-clock
    /// parity at the moment of the call — if the old wall-clock-stamping
    /// bug were still present, the disagreeing message would land in the
    /// wall-clock's slot instead of its own.
    #[test]
    fn decode_history_uses_message_own_parity_not_wall_clock() {
        let mut op = op_even("K5ARH");
        let evaluator = NullDxEvaluator;

        let wall_clock = SlotParity::current();
        let disagreeing = match wall_clock {
            SlotParity::Even => pancetta_core::slot::SlotParity::Odd,
            SlotParity::Odd => pancetta_core::slot::SlotParity::Even,
        };
        let agreeing = match wall_clock {
            SlotParity::Even => pancetta_core::slot::SlotParity::Even,
            SlotParity::Odd => pancetta_core::slot::SlotParity::Odd,
        };
        let expected_slot_for_disagreeing = match disagreeing {
            pancetta_core::slot::SlotParity::Even => TimeSlot::First,
            pancetta_core::slot::SlotParity::Odd => TimeSlot::Second,
        };
        let expected_slot_for_agreeing = match agreeing {
            pancetta_core::slot::SlotParity::Even => TimeSlot::First,
            pancetta_core::slot::SlotParity::Odd => TimeSlot::Second,
        };
        assert_ne!(
            expected_slot_for_disagreeing, expected_slot_for_agreeing,
            "sanity: disagreeing and agreeing parities must map to different TimeSlots"
        );

        let mut msg = dmi("K1ABC W1XYZ -12", Some("K1ABC"), 1500.0);
        msg.slot_parity = Some(disagreeing);

        op.feed_decoded_messages_at(&[msg], &evaluator, Utc::now());

        assert_eq!(
            op.decode_history
                .activity_near_in_slot(1500.0, 10.0, expected_slot_for_disagreeing),
            1,
            "message must be recorded under ITS OWN carried parity's slot"
        );
        assert_eq!(
            op.decode_history
                .activity_near_in_slot(1500.0, 10.0, expected_slot_for_agreeing),
            0,
            "message must NOT be recorded under the wall-clock's slot when \
             it disagrees with the message's own carried parity"
        );
    }

    /// FQ-F2 regression: messages with `slot_parity: None` (test scaffolding
    /// / untracked decodes) must keep falling back to the wall-clock-derived
    /// slot, unchanged from pre-fix behavior.
    #[test]
    fn decode_history_falls_back_to_wall_clock_when_parity_untracked() {
        let mut op = op_even("K5ARH");
        let evaluator = NullDxEvaluator;

        let wall_clock = SlotParity::current();
        let expected_slot = if wall_clock == SlotParity::Even {
            TimeSlot::First
        } else {
            TimeSlot::Second
        };

        let msg = dmi("K1ABC W1XYZ -12", Some("K1ABC"), 1500.0); // slot_parity: None
        op.feed_decoded_messages_at(&[msg], &evaluator, Utc::now());

        assert_eq!(
            op.decode_history
                .activity_near_in_slot(1500.0, 10.0, expected_slot),
            1,
            "untracked-parity message must fall back to the wall-clock slot"
        );
    }

    /// FQ-F2 (part 2): `record_slot_activity` (which feeds
    /// `SlotParityConfig::Auto`'s quieter-slot decision) must split a mixed
    /// batch's counts by each message's own carried parity, rather than
    /// lumping the whole batch's count under one wall-clock parity.
    #[test]
    fn record_slot_activity_splits_by_message_own_parity() {
        let config = AutonomousConfig {
            enabled: true,
            slot_parity: SlotParityConfig::Auto, // keep auto-detecting
            min_dx_score: 0.3,
            ..AutonomousConfig::default()
        };
        let mut op = AutonomousOperator::new(config, "K5ARH".into(), Some("FN42".into()));
        let evaluator = NullDxEvaluator;

        let mut even1 = dmi("K1ABC W1XYZ -12", Some("K1ABC"), 1500.0);
        even1.slot_parity = Some(pancetta_core::slot::SlotParity::Even);
        let mut even2 = dmi("K2ABC W2XYZ -12", Some("K2ABC"), 1600.0);
        even2.slot_parity = Some(pancetta_core::slot::SlotParity::Even);
        let mut odd1 = dmi("K3ABC W3XYZ -12", Some("K3ABC"), 1700.0);
        odd1.slot_parity = Some(pancetta_core::slot::SlotParity::Odd);
        let mut odd2 = dmi("K4ABC W4XYZ -12", Some("K4ABC"), 1800.0);
        odd2.slot_parity = Some(pancetta_core::slot::SlotParity::Odd);
        let mut odd3 = dmi("K5ABC W5XYZ -12", Some("K5ABC"), 1900.0);
        odd3.slot_parity = Some(pancetta_core::slot::SlotParity::Odd);

        op.feed_decoded_messages_at(&[even1, even2, odd1, odd2, odd3], &evaluator, Utc::now());

        assert_eq!(
            op.slot_manager.auto_detect_even_activity, 2,
            "2 Even-carried messages should be attributed to Even, \
             regardless of the batch's Odd majority or wall clock"
        );
        assert_eq!(
            op.slot_manager.auto_detect_odd_activity, 3,
            "3 Odd-carried messages should be attributed to Odd"
        );
    }
}
