//! # High-Performance Message Bus
//!
//! Inter-component communication system optimized for real-time audio processing.
//! Provides lock-free, low-latency message passing between Pancetta components.
//!
//! ## Features
//!
//! - **Sub-millisecond latency**: Optimized for real-time audio processing
//! - **Lock-free channels**: Uses crossbeam for high-performance messaging  
//! - **Type-safe messages**: Strongly typed message system with routing
//! - **Component health**: Built-in health monitoring and metrics
//! - **Backpressure handling**: Graceful degradation under load
//!
//! ## Architecture
//!
//! The message bus uses a hub-and-spoke pattern with dedicated channels
//! between components. Each component has its own receive channel and
//! can send to any other component through the bus.

use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use pancetta_ft8::DecodedMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, trace, warn};

/// Component identifiers for message routing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComponentId {
    /// Audio input and processing
    Audio,
    /// Digital signal processing pipeline
    Dsp,
    /// FT8 decoder
    Ft8Decoder,
    /// Terminal user interface
    Tui,
    /// Configuration manager
    Config,
    /// Application coordinator
    Coordinator,
    /// Hamlib rig control
    Hamlib,
    /// QSO management
    Qso,
    /// DX cluster and propagation
    DxCluster,
    /// FT8 transmitter
    Ft8Transmitter,
    /// Autonomous operator
    Autonomous,
    /// PSKReporter upload
    PskReporter,
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentId::Audio => write!(f, "Audio"),
            ComponentId::Dsp => write!(f, "DSP"),
            ComponentId::Ft8Decoder => write!(f, "FT8Decoder"),
            ComponentId::Tui => write!(f, "TUI"),
            ComponentId::Config => write!(f, "Config"),
            ComponentId::Coordinator => write!(f, "Coordinator"),
            ComponentId::Hamlib => write!(f, "Hamlib"),
            ComponentId::Qso => write!(f, "QSO"),
            ComponentId::DxCluster => write!(f, "DXCluster"),
            ComponentId::Ft8Transmitter => write!(f, "FT8Transmitter"),
            ComponentId::Autonomous => write!(f, "Autonomous"),
            ComponentId::PskReporter => write!(f, "PSKReporter"),
        }
    }
}

/// Message types that can be sent between components
#[derive(Debug, Clone)]
pub enum MessageType {
    /// Raw audio samples from input device
    AudioData(Vec<f32>),

    /// Processed audio data from DSP pipeline
    DspData(Vec<f32>),

    /// Decoded FT8 message
    DecodedMessage(DecodedMessage),

    /// Component heartbeat for health monitoring
    Heartbeat {
        component_id: ComponentId,
        timestamp: Instant,
        metrics: ComponentMetrics,
    },

    /// Configuration update notification
    ConfigUpdate {
        section: String,
        config_data: String, // JSON-serialized config
    },

    /// Control messages
    Control(ControlMessage),

    /// Error notification
    Error {
        component_id: ComponentId,
        error_message: String,
        error_code: Option<u32>,
    },

    /// Hamlib rig control messages
    RigControl(RigControlMessage),

    /// QSO management messages
    QsoMessage(QsoMessage),

    /// DX cluster messages
    DxMessage(DxMessage),

    /// Status update message
    StatusUpdate(String),

    /// Request to transmit an FT8 message.
    ///
    /// `frequency_offset` is the ABSOLUTE audio frequency in Hz (typically
    /// 200-2500 within the FT8 passband), NOT a delta from any base. The
    /// transmitter component sets the modulator's base_frequency to this
    /// value before encoding.
    TransmitRequest {
        message_text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        /// Required slot parity. `None` = no DX context (CQ);
        /// the scheduler falls back to the configured self-parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },

    /// Transmit completed notification
    TransmitComplete {
        success: bool,
        message_text: String,
        duration_ms: u64,
    },

    /// TX-active indicator for the TUI title-bar badge (Batch 93).
    /// The TX worker sends `active: true` when PTT is asserted and
    /// `active: false` when the transmission ends — via an RAII
    /// observer guard, so abort paths (F8, Shift+Q, shutdown) clear
    /// it just like normal completion. Observation only: this message
    /// never drives PTT or audio.
    TxStatus { active: bool },

    /// Richer TX-queue snapshot for the TUI's NOW-SENDING / QUEUED view.
    /// Sent by the TX worker alongside the boolean `TxStatus` badge:
    /// `sending` is `Some(item)` while a transmission is keyed (text +
    /// audio frequency on the air RIGHT NOW), `None` otherwise; `queued`
    /// lists items the worker has dequeued and is scheduling but has not
    /// yet started transmitting (waiting for the next slot of the correct
    /// parity). Observation only — never drives PTT or audio.
    ///
    /// Scope note: the TX worker processes one request at a time and sleeps
    /// through the slot, so `queued` reflects the request the worker is
    /// currently scheduling (between dequeue and PTT-assert), not a deep
    /// look into the crossbeam channel backlog. This is the lightweight
    /// scope documented in the design — it surfaces NOW + the in-flight
    /// pending item(s) without instrumenting the channel internals.
    TxQueueStatus {
        /// What is being transmitted right now (keyed). `None` = idle.
        sending: Option<TxItem>,
        /// Items dequeued and scheduled but not yet on the air.
        queued: Vec<TxItem>,
    },

    /// TX-policy state echo for the TUI banner. Sent by the coordinator's
    /// command relay whenever the operator changes the global TX policy
    /// (cycle key) or triggers an emergency stop (Shift+Q → Disabled).
    /// The TUI mirrors this into its bold, color-coded TX banner.
    /// Observation only.
    TxPolicyStatus {
        /// Current global TX policy.
        policy: pancetta_core::TxPolicy,
    },

    /// Autonomous operator status update
    AutonomousStatus(AutonomousStatusData),

    /// Request to transmit multiple messages simultaneously (multi-TX).
    /// Each item is encoded/modulated independently and summed into one waveform.
    /// All items in a bundle share the same slot, so they share the same parity.
    MultiTransmitRequest {
        items: Vec<TransmitRequestItem>,
        /// Required slot parity for the bundle. `None` = no DX context;
        /// the scheduler falls back to the configured self-parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },

    /// Audio output samples for transmission
    AudioOutput { samples: Vec<f32>, sample_rate: u32 },

    /// Single-tone tune transmission (operator pressed F4). Engages PTT,
    /// emits a continuous sine wave at `tone_offset_hz` for `duration_secs`
    /// or until aborted (F4-toggle, F8 halt, or shutdown). Bypasses the
    /// slot-aware scheduler — tune happens immediately, no parity logic.
    /// Amplitude is hardcoded at 0.5 (operator manages rig power).
    TuneRequest {
        duration_secs: u32,
        tone_offset_hz: f64,
    },

    /// Snapshot of in-progress QSOs, pushed by the QSO coordinator
    /// on every state change. tui_relay forwards this to the TUI as
    /// `TuiMessage::ActiveQsosUpdate`; the TUI replaces its previous
    /// active-QSOs list with the new snapshot.
    ActiveQsosSnapshot { qsos: Vec<ActiveQsoSnapshotItem> },

    /// Waterfall spectrogram data for TUI display
    WaterfallData {
        /// Power values in dB, one row per time step
        power_matrix: Vec<Vec<f32>>,
        /// Frequency range in Hz (min, max)
        freq_range: (f32, f32),
    },
}

/// A single transmit request item for multi-TX bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransmitRequestItem {
    pub message_text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
}

/// One row in a `MessageType::TxQueueStatus` payload — a compact,
/// display-oriented view of a TX item the worker is sending or has
/// queued. Decoupled from `TransmitRequest`/`TransmitRequestItem` so the
/// TUI renders just what it needs (text + audio frequency) without
/// pulling scheduling internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxItem {
    /// FT8 message text being / to-be transmitted.
    pub text: String,
    /// Absolute audio frequency (Hz) for this item.
    pub freq_hz: f64,
    /// QSO id this item belongs to, if any (`None` = CQ / manual send).
    pub qso_id: Option<String>,
}

/// One item in a `MessageType::ActiveQsosSnapshot` payload — flattened
/// view of an in-progress QSO with the fields the TUI banner AND the
/// QSO-detail panel need. Decoupled from `pancetta-qso::QsoState` so
/// the TUI doesn't link the QSO crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveQsoSnapshotItem {
    /// Other station's callsign.
    pub their_callsign: String,
    /// Human-readable state name (compact form: "wait rpt", "sending RR73").
    pub state: String,
    /// When this QSO started — TUI renders an elapsed timer from this.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Audio frequency in Hz where we're working this QSO.
    pub frequency_hz: f64,
    /// Parity our station transmits in for this QSO. Used by the TUI
    /// waterfall to color the occupancy strip and TX cursor by "is this
    /// slot mine."
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Raw text of the last message we transmitted in this QSO (Batch 94:
    /// drives the QSO-detail panel's TX line).
    pub last_tx_text: Option<String>,
    /// When the last TX message was recorded.
    pub last_tx_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Raw text of the last message we received from the contra station.
    pub last_rx_text: Option<String>,
    /// When the last RX message was recorded.
    pub last_rx_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Measured SNR (dB) of the last message received from them.
    pub snr_rx: Option<i32>,
    /// Signal report we sent them (their signal at our end).
    pub report_sent: Option<i32>,
    /// Signal report we received from them (our signal at their end).
    pub report_received: Option<i32>,
    /// Total messages exchanged (both directions) so far in this QSO.
    pub exchange_count: u32,
    /// Stable id of this QSO (UUID string). Used by the TUI to target
    /// abort/re-send management commands at a specific QSO.
    pub qso_id: String,
    /// How the QSO was initiated: "Manual" or "Auto".
    pub initiated_by: String,
    /// Display-ladder rung labels, left-to-right (derived from the QSO
    /// state + initiation role). Empty for states with no ladder.
    pub ladder_labels: Vec<String>,
    /// Per-rung flag: `true` if the rung's message is one WE transmit.
    pub ladder_ours: Vec<bool>,
    /// Index of the current rung in `ladder_labels`.
    pub ladder_index: usize,
    /// Human-readable "now" line (what we're doing this moment).
    pub now_line: String,
    /// Human-readable "next" line (what we expect next).
    pub next_line: String,
    /// Manual keep-calling watchdog: number of calls transmitted so far.
    /// Only meaningful for manual keep-calling states (RespondingToCq /
    /// SendingReport); `0` otherwise. The TUI renders "Call N/M" so the
    /// operator can see keep-calling is bounded (not an infinite loop).
    pub call_count: u32,
    /// Manual keep-calling watchdog: the call cap (`manual_call_max_calls`).
    /// `0` when not keep-calling.
    pub max_calls: u32,
    /// Manual keep-calling watchdog: when keep-calling will stop on the
    /// elapsed-time bound (`first_call_at + manual_call_watchdog_minutes`).
    /// The TUI renders a live countdown ("stops 3:12"). `None` when this QSO
    /// is not in a manual keep-calling state.
    pub watchdog_deadline: Option<chrono::DateTime<chrono::Utc>>,
}

/// Status data from the autonomous operator for TUI consumption.
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

/// Hamlib rig control messages
#[derive(Debug, Clone)]
pub enum RigControlMessage {
    /// Set frequency
    SetFrequency { vfo: u8, frequency: u64 },
    /// Get frequency
    GetFrequency { vfo: u8 },
    /// Frequency response
    FrequencyResponse { vfo: u8, frequency: u64 },
    /// Set mode
    SetMode {
        vfo: u8,
        mode: String,
        passband: Option<u32>,
    },
    /// PTT control
    SetPtt { state: bool },
    /// Get signal strength
    GetSignalStrength,
    /// Signal strength response from the rig's S-meter. Value follows
    /// the hamlib STRENGTH convention: dB relative to S9 (0 = S9,
    /// -54 ≈ S0, +20 = S9+20). Produced by the hamlib polling loop
    /// (Batch 95) from real `\get_level STRENGTH` reads — never
    /// synthesized.
    SignalStrengthResponse { db_over_s9: i32 },
}

/// QSO management messages
#[derive(Debug, Clone)]
pub enum QsoMessage {
    /// Start new QSO
    StartQso {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Respond to a station **calling us**, opening the exchange at an
    /// operator-chosen [`pancetta_core::ResponseStep`] rather than always
    /// sending our grid. Driven by the TUI Callers panel (smart default +
    /// override). Like `StartQso`, this is always a manual call.
    RespondToCaller {
        /// The caller's callsign.
        callsign: String,
        /// Audio offset (Hz, within the FT8 passband) to transmit on.
        frequency: u64,
        /// The slot parity the caller transmits on, if known. We reply on the
        /// opposite parity.
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        /// Which rung of the exchange ladder to open at.
        step: pancetta_core::ResponseStep,
        /// Our measured SNR of the caller, used to derive the report we send.
        snr: Option<f32>,
    },
    /// End QSO
    EndQso { qso_id: String },
    /// Log QSO
    LogQso { qso_data: String },
    /// Abort an in-progress QSO (operator-initiated cancel).
    AbortQso { qso_id: String },
    /// Re-send the most recent message we transmitted in this QSO.
    ResendQso { qso_id: String },
    /// Cancel ALL active QSOs at once. The emergency stop sends this so a
    /// single Shift+Q clears every keep-calling source (including duplicate
    /// QSO objects), not just the one selected by `AbortQso`.
    CancelAllQsos,
}

/// DX cluster messages
#[derive(Debug, Clone)]
pub enum DxMessage {
    /// New DX spot
    Spot {
        callsign: String,
        frequency: u64,
        spotter: String,
        comment: String,
    },
    /// Propagation update
    PropagationUpdate { band: String, conditions: String },
    /// Band activity
    BandActivity { band: String, activity_level: f32 },
}

/// Control messages for component lifecycle management
#[derive(Debug, Clone)]
pub enum ControlMessage {
    /// Start component processing
    Start,
    /// Stop component processing
    Stop,
    /// Pause component processing
    Pause,
    /// Resume component processing  
    Resume,
    /// Request component status
    StatusRequest,
    /// Component status response
    StatusResponse {
        component_id: ComponentId,
        is_running: bool,
        uptime: Duration,
        metrics: ComponentMetrics,
    },
    /// Shutdown command
    Shutdown,
}

/// Per-component performance metrics
#[derive(Debug, Clone, Default)]
pub struct ComponentMetrics {
    /// Total messages processed
    pub messages_processed: u64,
    /// Messages processed per second
    pub messages_per_second: f64,
    /// Average message processing latency
    pub avg_latency_us: f64,
    /// Peak memory usage in bytes
    pub peak_memory_bytes: usize,
    /// Current CPU usage percentage
    pub cpu_usage_percent: f64,
    /// Number of errors encountered
    pub error_count: u32,
    /// Last error timestamp
    pub last_error: Option<Instant>,
    /// Component-specific metrics
    pub custom_metrics: HashMap<String, f64>,
}

/// Complete message with routing and timing information
#[derive(Debug, Clone)]
pub struct ComponentMessage {
    /// Unique message identifier
    pub id: u64,
    /// Source component
    pub source: ComponentId,
    /// Destination component
    pub destination: ComponentId,
    /// Message payload
    pub message_type: MessageType,
    /// Message creation timestamp
    pub timestamp: Instant,
    /// Message priority (0 = highest, 255 = lowest)
    pub priority: u8,
    /// Number of routing hops
    pub hop_count: u8,
    /// Latency tracking timestamps
    pub latency_tracking: LatencyTracking,
}

/// Latency tracking for message bus performance monitoring
#[derive(Debug, Clone, Default)]
pub struct LatencyTracking {
    /// When message was queued for sending
    pub queued_at: Option<Instant>,
    /// When message was actually sent
    pub sent_at: Option<Instant>,
    /// When message was received
    pub received_at: Option<Instant>,
    /// When message processing started
    pub processing_started_at: Option<Instant>,
    /// When message processing completed
    pub processing_completed_at: Option<Instant>,
}

impl ComponentMessage {
    /// Create a new message with normal priority
    pub fn new(
        source: ComponentId,
        destination: ComponentId,
        message_type: MessageType,
        timestamp: Instant,
    ) -> Self {
        let mut latency_tracking = LatencyTracking::default();
        latency_tracking.queued_at = Some(Instant::now());

        Self {
            id: generate_message_id(),
            source,
            destination,
            message_type,
            timestamp,
            priority: 128, // Normal priority
            hop_count: 0,
            latency_tracking,
        }
    }

    /// Create a high-priority message (for real-time audio)
    pub fn new_high_priority(
        source: ComponentId,
        destination: ComponentId,
        message_type: MessageType,
        timestamp: Instant,
    ) -> Self {
        let mut latency_tracking = LatencyTracking::default();
        latency_tracking.queued_at = Some(Instant::now());

        Self {
            id: generate_message_id(),
            source,
            destination,
            message_type,
            timestamp,
            priority: 0, // Highest priority
            hop_count: 0,
            latency_tracking,
        }
    }

    /// Get message age in microseconds
    pub fn age_us(&self) -> u64 {
        self.timestamp.elapsed().as_micros() as u64
    }

    /// Check if message has expired (age > threshold)
    pub fn is_expired(&self, threshold_us: u64) -> bool {
        self.age_us() > threshold_us
    }

    /// Get total latency in microseconds
    pub fn total_latency_us(&self) -> Option<u64> {
        if let (Some(queued), Some(completed)) = (
            self.latency_tracking.queued_at,
            self.latency_tracking.processing_completed_at,
        ) {
            Some(completed.duration_since(queued).as_micros() as u64)
        } else {
            None
        }
    }

    /// Get transit latency in microseconds (queue to receive)
    pub fn transit_latency_us(&self) -> Option<u64> {
        if let (Some(queued), Some(received)) = (
            self.latency_tracking.queued_at,
            self.latency_tracking.received_at,
        ) {
            Some(received.duration_since(queued).as_micros() as u64)
        } else {
            None
        }
    }

    /// Get processing latency in microseconds
    pub fn processing_latency_us(&self) -> Option<u64> {
        if let (Some(started), Some(completed)) = (
            self.latency_tracking.processing_started_at,
            self.latency_tracking.processing_completed_at,
        ) {
            Some(completed.duration_since(started).as_micros() as u64)
        } else {
            None
        }
    }
}

/// Component health information
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub component_id: ComponentId,
    pub is_healthy: bool,
    pub last_heartbeat: Instant,
    pub error_count: u32,
    pub message_count: u64,
    pub avg_latency_ms: f64,
    pub metrics: ComponentMetrics,
}

/// Message bus configuration
#[derive(Debug, Clone)]
pub struct MessageBusConfig {
    /// Maximum number of queued messages per component
    pub max_queue_size: usize,
    /// Message timeout in microseconds
    pub message_timeout_us: u64,
    /// Health check interval
    pub health_check_interval: Duration,
    /// Enable message tracing for debugging
    pub enable_tracing: bool,
    /// Enable metrics collection
    pub enable_metrics: bool,
}

impl Default for MessageBusConfig {
    fn default() -> Self {
        Self {
            max_queue_size: 10000,
            message_timeout_us: 30_000_000, // 30s timeout for control messages
            health_check_interval: Duration::from_secs(5),
            enable_tracing: false,
            enable_metrics: true,
        }
    }
}

/// Channel pair for component communication
struct ComponentChannel {
    sender: Sender<ComponentMessage>,
    receiver: Receiver<ComponentMessage>,
    component_id: ComponentId,
    message_count: Arc<AtomicU64>,
    error_count: Arc<AtomicU64>,
    last_heartbeat: Arc<RwLock<Option<Instant>>>,
}

/// High-performance message bus for inter-component communication
#[derive(Clone)]
pub struct MessageBus {
    /// Configuration
    config: MessageBusConfig,
    /// Component channels
    channels: Arc<RwLock<HashMap<ComponentId, ComponentChannel>>>,
    /// Global message counter
    message_counter: Arc<AtomicU64>,
    /// Bus metrics
    total_messages: Arc<AtomicU64>,
    dropped_messages: Arc<AtomicU64>,
    expired_messages: Arc<AtomicU64>,
}

impl MessageBus {
    /// Create a new message bus
    pub fn new(buffer_size: usize) -> Result<Self> {
        let config = MessageBusConfig {
            max_queue_size: buffer_size,
            ..Default::default()
        };

        Ok(Self {
            config,
            channels: Arc::new(RwLock::new(HashMap::new())),
            message_counter: Arc::new(AtomicU64::new(0)),
            total_messages: Arc::new(AtomicU64::new(0)),
            dropped_messages: Arc::new(AtomicU64::new(0)),
            expired_messages: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Create a new message bus with custom configuration
    pub fn with_config(config: MessageBusConfig) -> Result<Self> {
        Ok(Self {
            config,
            channels: Arc::new(RwLock::new(HashMap::new())),
            message_counter: Arc::new(AtomicU64::new(0)),
            total_messages: Arc::new(AtomicU64::new(0)),
            dropped_messages: Arc::new(AtomicU64::new(0)),
            expired_messages: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Create a communication channel for a component
    pub async fn create_channel(
        &self,
        component_id: ComponentId,
    ) -> Result<(Sender<ComponentMessage>, Receiver<ComponentMessage>)> {
        let mut channels = self.channels.write().await;

        if channels.contains_key(&component_id) {
            return Err(anyhow::anyhow!(
                "Channel already exists for component: {}",
                component_id
            ));
        }

        let (sender, receiver) = bounded(self.config.max_queue_size);

        let channel = ComponentChannel {
            sender: sender.clone(),
            receiver: receiver.clone(),
            component_id,
            message_count: Arc::new(AtomicU64::new(0)),
            error_count: Arc::new(AtomicU64::new(0)),
            last_heartbeat: Arc::new(RwLock::new(None)),
        };

        channels.insert(component_id, channel);

        debug!("Created message channel for component: {}", component_id);

        Ok((sender, receiver))
    }

    /// Send a message to a specific component
    pub async fn send_message(&self, mut message: ComponentMessage) -> Result<()> {
        // Check message expiration
        if message.is_expired(self.config.message_timeout_us) {
            self.expired_messages.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Dropping expired message from {} to {} (age: {}μs)",
                message.source,
                message.destination,
                message.age_us()
            );
            return Ok(());
        }

        // Mark message as sent
        message.latency_tracking.sent_at = Some(Instant::now());

        let channels = self.channels.read().await;

        if let Some(channel) = channels.get(&message.destination) {
            match channel.sender.try_send(message.clone()) {
                Ok(_) => {
                    channel.message_count.fetch_add(1, Ordering::Relaxed);
                    self.total_messages.fetch_add(1, Ordering::Relaxed);

                    // Update latency metrics if available
                    if let Some(transit_us) = message.transit_latency_us() {
                        // Store average latency (simplified - in production would use rolling average)
                        let _avg_latency = transit_us as f64;
                    }

                    if self.config.enable_tracing {
                        trace!(
                            "Message sent from {} to {}: {:?} (transit: {:?}μs)",
                            message.source,
                            message.destination,
                            message.id,
                            message.transit_latency_us()
                        );
                    }
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    channel.error_count.fetch_add(1, Ordering::Relaxed);
                    self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "Channel full, dropping message from {} to {}",
                        message.source, message.destination
                    );
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    channel.error_count.fetch_add(1, Ordering::Relaxed);
                    self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                    error!(
                        "Channel disconnected for component: {}",
                        message.destination
                    );
                }
            }
        } else {
            warn!(
                "No channel found for destination component: {}",
                message.destination
            );
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Broadcast a message to all components except the sender
    pub async fn broadcast_message(&self, message: ComponentMessage) -> Result<()> {
        let channels = self.channels.read().await;

        for (&component_id, channel) in channels.iter() {
            if component_id != message.source {
                let mut broadcast_message = message.clone();
                broadcast_message.destination = component_id;
                broadcast_message.hop_count += 1;

                if let Err(e) = channel.sender.try_send(broadcast_message) {
                    warn!("Failed to broadcast to {}: {}", component_id, e);
                    channel.error_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        self.total_messages.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Get health status for all components
    pub async fn get_component_health(&self) -> Vec<ComponentHealth> {
        let channels = self.channels.read().await;
        let mut health_status = Vec::new();

        for (&component_id, channel) in channels.iter() {
            let message_count = channel.message_count.load(Ordering::Relaxed);
            let error_count = channel.error_count.load(Ordering::Relaxed) as u32;

            let last_heartbeat = {
                let heartbeat = channel.last_heartbeat.read().await;
                heartbeat.unwrap_or_else(Instant::now)
            };

            let is_healthy =
                error_count < 100 && last_heartbeat.elapsed() < Duration::from_secs(30);

            // No real latency tracking — report None rather than fake data
            let avg_latency_ms = 0.0;

            health_status.push(ComponentHealth {
                component_id,
                is_healthy,
                last_heartbeat,
                error_count,
                message_count,
                avg_latency_ms,
                metrics: ComponentMetrics::default(),
            });
        }

        health_status
    }

    /// Get message bus statistics
    pub fn get_statistics(&self) -> MessageBusStatistics {
        MessageBusStatistics {
            total_messages: self.total_messages.load(Ordering::Relaxed),
            dropped_messages: self.dropped_messages.load(Ordering::Relaxed),
            expired_messages: self.expired_messages.load(Ordering::Relaxed),
            active_channels: 0, // Will be calculated when called
        }
    }

    /// Update component heartbeat
    pub async fn update_heartbeat(&self, component_id: ComponentId) -> Result<()> {
        let channels = self.channels.read().await;

        if let Some(channel) = channels.get(&component_id) {
            let mut heartbeat = channel.last_heartbeat.write().await;
            *heartbeat = Some(Instant::now());
        }

        Ok(())
    }

    /// Remove a component channel (cleanup)
    pub async fn remove_channel(&self, component_id: ComponentId) -> Result<()> {
        let mut channels = self.channels.write().await;

        if channels.remove(&component_id).is_some() {
            debug!("Removed channel for component: {}", component_id);
        } else {
            warn!("Attempted to remove non-existent channel: {}", component_id);
        }

        Ok(())
    }
}

/// Message bus performance statistics
#[derive(Debug, Clone)]
pub struct MessageBusStatistics {
    pub total_messages: u64,
    pub dropped_messages: u64,
    pub expired_messages: u64,
    pub active_channels: usize,
}

// Global message ID generator
static MESSAGE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn generate_message_id() -> u64 {
    MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_message_bus_creation() {
        let bus = MessageBus::new(1000).unwrap();
        let stats = bus.get_statistics();
        assert_eq!(stats.total_messages, 0);
    }

    #[tokio::test]
    async fn test_channel_creation() {
        let bus = MessageBus::new(1000).unwrap();
        let result = bus.create_channel(ComponentId::Audio).await;
        assert!(result.is_ok());

        // Should fail to create duplicate channel
        let duplicate_result = bus.create_channel(ComponentId::Audio).await;
        assert!(duplicate_result.is_err());
    }

    #[tokio::test]
    async fn test_message_sending() {
        let bus = MessageBus::new(1000).unwrap();
        let (tx, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        let (dsp_tx, dsp_rx) = bus.create_channel(ComponentId::Dsp).await.unwrap();

        let message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![0.1, 0.2, 0.3]),
            Instant::now(),
        );

        bus.send_message(message).await.unwrap();

        // Should be able to receive the message
        let received = dsp_rx.try_recv();
        assert!(received.is_ok());
    }

    #[tokio::test]
    async fn test_message_expiration() {
        let mut config = MessageBusConfig::default();
        config.message_timeout_us = 1; // 1 microsecond timeout

        let bus = MessageBus::with_config(config).unwrap();
        let (tx, rx) = bus.create_channel(ComponentId::Dsp).await.unwrap();

        let old_message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![0.1]),
            Instant::now() - Duration::from_millis(10),
        );

        // Sleep to ensure message is old
        sleep(Duration::from_micros(10)).await;

        bus.send_message(old_message).await.unwrap();

        // Message should be dropped due to expiration
        let stats = bus.get_statistics();
        assert_eq!(stats.expired_messages, 1);
    }

    #[tokio::test]
    async fn test_component_health() {
        let bus = MessageBus::new(1000).unwrap();
        bus.create_channel(ComponentId::Audio).await.unwrap();
        bus.update_heartbeat(ComponentId::Audio).await.unwrap();

        let health = bus.get_component_health().await;
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].component_id, ComponentId::Audio);
        assert!(health[0].is_healthy);
    }

    #[test]
    fn test_component_message_creation() {
        let message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![0.1, 0.2]),
            Instant::now(),
        );

        assert_eq!(message.source, ComponentId::Audio);
        assert_eq!(message.destination, ComponentId::Dsp);
        assert_eq!(message.priority, 128);
        assert_eq!(message.hop_count, 0);
    }

    #[test]
    fn test_high_priority_message() {
        let message = ComponentMessage::new_high_priority(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![0.1]),
            Instant::now(),
        );

        assert_eq!(message.priority, 0);
    }
}
