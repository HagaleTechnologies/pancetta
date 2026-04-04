//! Autonomous QSO Operator
//!
//! Sits above the existing `AutoSequencer` and `QsoManager`, making cycle-by-cycle
//! decisions: hunt for interesting CQs, call CQ when idle, manage even/odd slots,
//! and periodically listen on our TX slot to detect doubling.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::frequency::{
    DecodeHistory, DecodeRecord, FrequencyAllocatorConfig, SmartFrequencyAllocator,
    SpectralSnapshot, TimeSlot,
};

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
    pub max_concurrent_qsos: u32,
    pub tx_offset_hz: f64,
    /// 0.0–1.0 threshold for DX score when deciding whether to answer a CQ.
    pub min_dx_score: f64,
    /// Directed CQ text (e.g. "DX", "NA", or empty).
    pub cq_direction: String,
    pub listen_cycle: ListenCycleConfig,
    pub band_hopping: BandHoppingConfig,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slot_parity: SlotParityConfig::Auto,
            cq_after_idle_cycles: 10,
            max_concurrent_qsos: 1,
            tx_offset_hz: 1500.0,
            min_dx_score: 0.3,
            cq_direction: String::new(),
            listen_cycle: ListenCycleConfig::default(),
            band_hopping: BandHoppingConfig::default(),
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
        let mut best_clearance = 0.0f64;

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
    /// Minimum score to open an additional QSO slot.
    pub min_multi_slot_score: f64,
    /// Whether the user has paused autonomous operation.
    paused: bool,
}

impl AutonomousOperator {
    pub fn new(config: AutonomousConfig, our_callsign: String, our_grid: Option<String>) -> Self {
        let slot_manager = SlotManager::new(config.slot_parity, &config.listen_cycle);
        let collision_detector = CollisionDetector::new(config.tx_offset_hz, 50.0);
        let band_strategy = BandStrategy::new(config.band_hopping.clone());
        // FT8 bandwidth: 8 tones * 6.25 Hz = 50 Hz, plus 25 Hz guard = 75 Hz min separation
        let frequency_allocator = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        let decode_history = DecodeHistory::new(4);
        let smart_allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());

        Self {
            config,
            slot_manager,
            collision_detector,
            band_strategy,
            frequency_allocator,
            state: OperatingState::Hunting,
            idle_cycles: 0,
            our_callsign,
            our_grid,
            pending_cqs: Vec::new(),
            active_qso_count: 0,
            pending_sequencer_messages: Vec::new(),
            decode_history,
            spectral_snapshot: None,
            smart_allocator,
            min_multi_slot_score: 0.7,
            paused: false,
        }
    }

    // -- external inputs ----------------------------------------------------

    /// Feed decoded messages from the most recent RX slot so the operator
    /// can score CQs and check for collisions.
    pub fn feed_decoded_messages(
        &mut self,
        messages: &[DecodedMessageInfo],
        evaluator: &dyn DxEvaluator,
    ) {
        // Auto-parity detection.
        let current_parity = SlotParity::current();
        self.slot_manager
            .record_slot_activity(current_parity, messages.len() as u32);

        // Band-hopping activity tracking.
        self.band_strategy.record_activity(messages.len() as u32);

        // Update frequency allocator with observed activity.
        self.frequency_allocator.update_observed(messages);

        // Record decode history for smart frequency allocation.
        let current_slot = if SlotParity::current() == SlotParity::Even {
            TimeSlot::First
        } else {
            TimeSlot::Second
        };
        let records: Vec<DecodeRecord> = messages
            .iter()
            .map(|m| DecodeRecord {
                frequency_hz: m.frequency_hz,
                time_slot: current_slot,
            })
            .collect();
        self.decode_history.push_cycle(records);

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

                    self.pending_cqs.push(CqCandidate {
                        callsign: call.clone(),
                        grid,
                        snr,
                        frequency_hz: msg.frequency_hz,
                        dx_score: score,
                    });
                }
            }
        }

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

    /// Get the best frequency for a new QSO using the smart allocator.
    /// Falls back to the legacy allocator if no spectral data is available.
    fn allocate_smart_frequency(&self, dx_target_hz: Option<f64>) -> f64 {
        let own_freqs: Vec<f64> = self.frequency_allocator.own_frequencies().values().copied().collect();

        if let Some(ref spectral) = self.spectral_snapshot {
            let candidates = self.smart_allocator.rank_candidates(
                spectral,
                &self.decode_history,
                &own_freqs,
                dx_target_hz,
            );
            if let Some(best) = candidates.first() {
                return best.offset_hz;
            }
        }

        // Fallback: legacy allocator
        self.frequency_allocator.allocate_cq_frequency()
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
    pub fn decide_at(&mut self, unix_secs: i64) -> Vec<OperatorAction> {
        let mut actions = Vec::new();

        if self.paused {
            actions.push(self.status_action());
            return actions;
        }

        // Step 0: band hopping
        if let Some(new_freq) = self.band_strategy.should_hop() {
            actions.push(OperatorAction::ChangeBand {
                dial_frequency: new_freq,
            });
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
                        });
                        tx_count += 1;
                    }
                    self.state = OperatingState::InQso {
                        qso_count: self.active_qso_count,
                    };
                    self.idle_cycles = 0;
                }

                // Step 3: if we have capacity, try to respond to a CQ or call CQ.
                let can_add_new = tx_count < self.config.max_concurrent_qsos
                    && self.active_qso_count < self.config.max_concurrent_qsos;

                if can_add_new {
                    // Try interesting CQs first.
                    let best_cq = self
                        .pending_cqs
                        .iter()
                        .filter(|cq| cq.dx_score >= self.config.min_dx_score)
                        .find(|cq| self.frequency_allocator.is_clear_of_own(cq.frequency_hz))
                        .cloned();

                    if let Some(cq) = best_cq {
                        if tx_count == 0 {
                            self.state = OperatingState::Hunting;
                        }
                        self.idle_cycles = 0;

                        let grid_part = self
                            .our_grid
                            .as_deref()
                            .map(|g| format!(" {}", g))
                            .unwrap_or_default();
                        let message_text =
                            format!("{} {} {}", cq.callsign, self.our_callsign, grid_part)
                                .trim()
                                .to_string();

                        debug!(
                            "Responding to CQ from {} (score={:.2}, snr={}) at {:.0} Hz",
                            cq.callsign, cq.dx_score, cq.snr, cq.frequency_hz
                        );

                        actions.push(OperatorAction::Transmit {
                            message_text,
                            frequency_offset: cq.frequency_hz,
                            qso_id: None,
                        });
                        tx_count += 1;
                    } else if tx_count == 0 {
                        // Step 4: no CQs worth answering and no active QSOs — CQ ourselves?
                        self.idle_cycles += 1;

                        if self.idle_cycles >= self.config.cq_after_idle_cycles {
                            self.state = OperatingState::CallingCq;
                            self.idle_cycles = 0;

                            let cq_freq = self.frequency_allocator.allocate_cq_frequency();

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

            // Pick a new offset with random jitter.
            let jitter = simple_jitter();
            let new_offset = (self.config.tx_offset_hz + jitter).clamp(200.0, 2800.0);
            self.set_tx_offset(new_offset);

            actions.push(OperatorAction::FrequencyShift {
                new_offset_hz: new_offset,
            });
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

fn extract_grid_from_cq(text: &str) -> Option<String> {
    // CQ messages: "CQ W1ABC FN42" or "CQ DX W1ABC FN42"
    let parts: Vec<&str> = text.split_whitespace().collect();
    // The grid is the last token if it looks like a Maidenhead locator (2 letters + 2 digits).
    if let Some(last) = parts.last() {
        if last.len() >= 4
            && last.len() <= 6
            && last
                .chars()
                .nth(0)
                .map_or(false, |c| c.is_ascii_alphabetic())
            && last
                .chars()
                .nth(1)
                .map_or(false, |c| c.is_ascii_alphabetic())
            && last.chars().nth(2).map_or(false, |c| c.is_ascii_digit())
            && last.chars().nth(3).map_or(false, |c| c.is_ascii_digit())
        {
            return Some(last.to_uppercase());
        }
    }
    None
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
        }];

        let result = detector.check_for_collision(&messages);
        assert!(result.detected);
        assert_eq!(result.interfering_calls, vec!["K1DEF".to_string()]);
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
}
