//! Autonomous QSO Operator
//!
//! Sits above the existing `AutoSequencer` and `QsoManager`, making cycle-by-cycle
//! decisions: hunt for interesting CQs, call CQ when idle, manage even/odd slots,
//! and periodically listen on our TX slot to detect doubling.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info, warn};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotParityConfig {
    Even,
    Odd,
    /// Listen for a few slots and pick the quieter parity.
    Auto,
}

impl Default for SlotParityConfig {
    fn default() -> Self {
        SlotParityConfig::Auto
    }
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
                self.our_slot.unwrap(),
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
    state: OperatingState,
    idle_cycles: u32,
    our_callsign: String,
    our_grid: Option<String>,
    /// CQs decoded in the most recent RX slot.
    pending_cqs: Vec<CqCandidate>,
    /// Number of active QSOs (tracked externally, fed in).
    active_qso_count: u32,
    /// Messages to transmit from the auto-sequencer (fed in).
    pending_sequencer_message: Option<(String, Option<String>)>,
    /// Whether the user has paused autonomous operation.
    paused: bool,
}

impl AutonomousOperator {
    pub fn new(config: AutonomousConfig, our_callsign: String, our_grid: Option<String>) -> Self {
        let slot_manager = SlotManager::new(config.slot_parity, &config.listen_cycle);
        let collision_detector = CollisionDetector::new(config.tx_offset_hz, 50.0);
        let band_strategy = BandStrategy::new(config.band_hopping.clone());

        Self {
            config,
            slot_manager,
            collision_detector,
            band_strategy,
            state: OperatingState::Hunting,
            idle_cycles: 0,
            our_callsign,
            our_grid,
            pending_cqs: Vec::new(),
            active_qso_count: 0,
            pending_sequencer_message: None,
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

    /// Tell the operator how many QSOs the auto-sequencer is currently managing.
    pub fn set_active_qso_count(&mut self, count: u32) {
        self.active_qso_count = count;
    }

    /// Feed a message the auto-sequencer wants to send this cycle.
    pub fn set_pending_sequencer_message(&mut self, message_text: String, qso_id: Option<String>) {
        self.pending_sequencer_message = Some((message_text, qso_id));
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
                // Step 2: active QSOs?
                if self.active_qso_count > 0 {
                    if let Some((msg, qso_id)) = self.pending_sequencer_message.take() {
                        self.state = OperatingState::InQso {
                            qso_count: self.active_qso_count,
                        };
                        self.idle_cycles = 0;
                        actions.push(OperatorAction::Transmit {
                            message_text: msg,
                            frequency_offset: self.config.tx_offset_hz,
                            qso_id,
                        });
                    } else {
                        // Sequencer has nothing to send — just listen.
                        actions.push(OperatorAction::Listen);
                    }
                } else {
                    // Step 3: any interesting CQs?
                    let best_cq = self
                        .pending_cqs
                        .first()
                        .filter(|cq| cq.dx_score >= self.config.min_dx_score)
                        .cloned();

                    if let Some(cq) = best_cq {
                        self.state = OperatingState::Hunting;
                        self.idle_cycles = 0;

                        // Build the CQ response message.
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
                            "Responding to CQ from {} (score={:.2}, snr={})",
                            cq.callsign, cq.dx_score, cq.snr
                        );

                        actions.push(OperatorAction::Transmit {
                            message_text,
                            frequency_offset: cq.frequency_hz,
                            qso_id: None,
                        });
                    } else {
                        // Step 4: idle long enough to CQ?
                        self.idle_cycles += 1;

                        if self.idle_cycles >= self.config.cq_after_idle_cycles {
                            self.state = OperatingState::CallingCq;
                            self.idle_cycles = 0;

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
                                frequency_offset: self.config.tx_offset_hz,
                                qso_id: None,
                            });
                        } else {
                            self.state = OperatingState::Hunting;
                            actions.push(OperatorAction::Listen);
                        }
                    }
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
}
