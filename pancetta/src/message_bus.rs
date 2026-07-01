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
    /// Read-only remote view gateway (Panino client)
    RemoteGateway,
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
            ComponentId::RemoteGateway => write!(f, "remote_gateway"),
        }
    }
}

/// Where a transmit request originated.
///
/// This is a **safety-relevant** discriminator: the coordinator's TX worker
/// applies the station-agent remote-TX arm gate **only** to
/// [`TxOrigin::Remote`] requests. Every request built by the local pipeline
/// (QSO state machine, autonomous operator, TUI, tune/test) is
/// [`TxOrigin::Local`] and skips the arm check entirely, so local TX behavior
/// is byte-identical to before this gate existed.
///
/// **`Default` is [`TxOrigin::Local`]** — the fail-safe direction: a request
/// built without explicitly setting `origin` is treated as local (subject to
/// the normal `TxPolicy` + drop-stale gates, never the remote arm). A remote
/// request must *explicitly* opt into `Remote`, and until the P3 relay wire
/// exists nothing constructs a `Remote` request, so the gate is inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TxOrigin {
    /// Locally-originated TX (QSO engine, autonomous, TUI, tune/test). Never
    /// subject to the remote-TX arm gate — byte-identical to legacy behavior.
    #[default]
    Local,
    /// Remote-originated TX (station-agent relay). Gated by the coordinator's
    /// `ArmState::tx_permitted()` before PTT is keyed; dropped (fail-closed) if
    /// not armed/permitted.
    Remote,
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
        /// Where this request came from. `Local` (default) skips the
        /// remote-TX arm gate; `Remote` is gated by `ArmState::tx_permitted()`.
        #[allow(dead_code)]
        origin: TxOrigin,
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

    /// Split-TX state echo for the TUI title-bar chip. Sent by the
    /// coordinator relay after every write to the split atomic: on
    /// `TuiCommand::SetSplit` (operator modal), on manual band-change
    /// (clears split), and on autonomous band-hop (clears split).
    /// `tx_hz == 0` means simplex (chip hidden). Observation only.
    SplitStatus {
        /// Current split TX frequency in Hz, or 0 for simplex.
        tx_hz: u64,
    },

    /// Fox-mode state echo for the TUI FOX chip.  Sent by the `SetFoxMode`
    /// handler on every path (successful engage, refused engage, or disengage)
    /// so the TUI `fox_mode` flag is always authoritative.  Without this the
    /// TUI's optimistic Shift+X flip can desync when engage is refused under
    /// RespondOnly/Disabled TX policy.  Observation only.
    FoxModeStatus {
        /// `true` if Fox mode was actually engaged; `false` if refused or
        /// disengaged.
        on: bool,
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
        /// Where this bundle came from. `Local` (default) skips the remote-TX
        /// arm gate; `Remote` is gated by `ArmState::tx_permitted()`. The whole
        /// bundle shares one origin (a bundle is one operator's slot).
        #[allow(dead_code)]
        origin: TxOrigin,
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
    ActiveQsosSnapshot {
        qsos: Vec<ActiveQsoSnapshotItem>,
        /// Cross-parity manual calls parked in the queue (#40), waiting for
        /// the active TX window to clear before they can start. Included in
        /// the same push so the TUI always sees a consistent (active, queued)
        /// pair without a separate message.
        pending: Vec<PendingCallSnapshotItem>,
    },

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
    /// `true` when this item missed its target slot and was deferred to a
    /// later slot (the WSJT-X-style late-TX 30s defer). Lets the TUI strip
    /// show "QUEUED → deferred 30s" instead of looking dead.
    #[serde(default)]
    pub deferred: bool,
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
    /// #41: a short summary of what the DX is doing on the band right now,
    /// from their latest decoded frame (e.g. "CQ", "→ W1XYZ R-12", "→ us -09").
    /// Lets the operator see — even before the DX answers — whether they're
    /// busy working someone else, calling CQ, or coming back to us. `None`
    /// when we've heard nothing recent from them.
    pub dx_last_activity: Option<String>,
    /// `true` when this QSO is using the FT8 DXpedition Hound procedure:
    /// call low (300–900 Hz), QSY up (>1000 Hz) on the Fox's report,
    /// complete on RR73. Additive — `false` for all normal QSOs.
    pub hound: bool,
}

/// One entry in the cross-parity manual-call queue (#40), included in
/// `MessageType::ActiveQsosSnapshot::pending`. The TUI renders these as a
/// compact "Queued" section so the operator knows a cross-window call is
/// waiting rather than silently dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCallSnapshotItem {
    /// DX callsign the operator chose to work.
    pub callsign: String,
    /// The DX's slot parity (the window they transmit in; we would reply on
    /// the opposite). `None` is theoretically possible but in practice this
    /// is always `Some` for queued calls.
    pub dx_parity: Option<pancetta_core::slot::SlotParity>,
    /// How long this call has been waiting (wall-clock seconds since it was
    /// parked in the queue).
    pub waited_secs: u64,
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
    /// Live SWR reading (e.g. 1.3 = 1.3:1), from the hamlib polling loop's
    /// `\get_level SWR` reads while PTT is keyed. Only meaningful during TX.
    SwrResponse { swr: f32 },
    /// Enable/disable rig-level split (RX dial ≠ TX dial). When `enabled`,
    /// the rig transmits on VFO B at `tx_frequency` (Hz) while receiving on
    /// VFO A. When disabled, `tx_frequency` is ignored. Produced by the TUI
    /// SetSplit relay; consumed by the hamlib command loop.
    SetSplit { enabled: bool, tx_frequency: u64 },
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
    /// Start a **manual** CQ call as a real `CallingCq` QSO (operator `c`).
    ///
    /// Unlike the autonomous CQ, this creates a tracked QSO that keeps
    /// calling CQ every slot (under the manual watchdog) and — when a station
    /// answers — auto-sequences the exchange through to Completed + ADIF log.
    /// Stopped by [`QsoMessage::StopCq`] (operator `s`).
    StartCq {
        /// Audio offset (Hz, within the FT8 passband) to transmit our CQ on.
        frequency: u64,
        /// The slot parity we want our CQ to land on (we choose our own when
        /// calling CQ). `None` lets the TX scheduler pick via the configured
        /// self-parity fallback.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Stop the manual CQ: cancel any active `CallingCq` QSO that has not yet
    /// been answered (operator `s`). A `CallingCq` QSO that already advanced
    /// (a caller answered) is left alone so the in-progress exchange finishes.
    StopCq,
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
    /// The operator changed bands (rig dial frequency) mid-session (C9). The
    /// QSO component tears down every active QSO — an active QSO cannot
    /// complete on a different band, and its keep-call must NOT keep
    /// transmitting on the new band. Carries the old/new dial frequency (Hz)
    /// for an operator-facing status line ("Band change — N active QSO(s)
    /// ended"). Distinct from [`QsoMessage::CancelAllQsos`] only so the status
    /// text and log target can name the cause.
    BandChanged {
        /// Previous dial frequency in Hz.
        previous_hz: u64,
        /// New dial frequency in Hz.
        new_hz: u64,
    },
    /// Open an **autonomous** QSO (`CallInitiation::Auto`) from the autonomous
    /// operator's decision (Phase 5). Routed to the QSO component so the
    /// `QsoManager` owns the exchange and auto-sequences it to completion,
    /// exactly as the manual `StartQso` path does for manual calls.
    ///
    /// The `QsoManager` emits the **opening** `MessageToSend` (forwarded to the
    /// transmitter by the QSO event loop) and a `StateChanged` (which populates
    /// the coordinator's `active_tx_qsos` set). The autonomous task therefore
    /// must **not** also transmit the opening itself — it sends this message
    /// *instead of* its raw opening `TransmitRequest` (no double-send). All of
    /// the autonomous task's gating (Shift+Q runtime gate, tri-state TX policy
    /// initiation suppression, dry-run) is applied *before* this message is
    /// sent, so a suppressed cycle never creates a QSO.
    StartAutonomousQso {
        /// `Some(dx)` = a hunt/pounce on a station calling CQ (→
        /// `QsoManager::respond_to_cq`). `None` = we are calling CQ ourselves
        /// (→ `QsoManager::start_cq`).
        callsign: Option<String>,
        /// For a pounce: the DX's **decoded** audio frequency (we answer
        /// Tx=Rx so the DX's subsequent frames pass the QSO relevance gate).
        /// For a CQ: our chosen audio offset.
        frequency: f64,
        /// For a pounce: the DX's slot parity (we reply on the opposite). For
        /// a CQ: our chosen TX parity (`None` → self-parity fallback).
        parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Enable or disable Fox (DXpedition operator) mode.
    ///
    /// `on: true` — sets the `fox_mode` flag, starts a repeating CQ
    /// (`CallingCq` QSO), and raises the concurrent caller-answer cap to
    /// `fox_max_streams` so the Fox can work many Hound callers at once.
    /// TX-policy gated (Fox originates CQ = initiation): refused under
    /// `RespondOnly` / `Disabled`, matching `StartCq` / `CallStation`.
    ///
    /// `on: false` — clears the flag, cancels any active un-answered `CallingCq`
    /// QSO (same as `StopCq`), and restores the normal caller-answer cap.
    SetFoxMode {
        /// `true` to engage Fox mode; `false` to disengage.
        on: bool,
    },
    /// Operator engaged Hound (DXpedition chaser) mode on a selected Fox.
    ///
    /// Opens a manual Hound QSO: calls the Fox low (300–900 Hz), QSYs up into
    /// the Hound-response region (>1000 Hz) when the Fox sends us a report,
    /// completes on the Fox's RR73, and flags the ADIF log record with
    /// `HOUND`/`APP_PANCETTA_HOUND`. Uses the same half-duplex parity admit
    /// gate and `PendingManualCalls` deferral as [`QsoMessage::StartQso`].
    EngageHound {
        /// The Fox's callsign.
        callsign: String,
        /// The Fox's RX audio offset (Hz) — where we hear the Fox. Used as
        /// the `partner_freq` for the relevance gate and decoder bin-hint.
        fox_freq: u64,
        /// The Fox's slot parity (we TX on the opposite parity). `None` when
        /// the Fox's parity is not yet known; the TX scheduler falls back to
        /// the configured self-parity.
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        /// The Fox's Maidenhead grid square, if known (for logging only).
        fox_grid: Option<String>,
    },
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
        let latency_tracking = LatencyTracking {
            queued_at: Some(Instant::now()),
            ..LatencyTracking::default()
        };

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
            // Perf (Pass 1 / A8): capture the small Copy fields needed for
            // metrics/tracing/error logs up front so the message can be MOVED
            // into try_send instead of deep-cloned on EVERY point-to-point send.
            // The old `try_send(message.clone())` cloned the whole
            // ComponentMessage — including potentially large MessageType
            // payloads (audio buffers, snapshots) — on the hot bus path.
            let src = message.source;
            let dst = message.destination;
            let msg_id = message.id;
            let transit = message.transit_latency_us();
            match channel.sender.try_send(message) {
                Ok(_) => {
                    channel.message_count.fetch_add(1, Ordering::Relaxed);
                    self.total_messages.fetch_add(1, Ordering::Relaxed);

                    if self.config.enable_tracing {
                        trace!(
                            "Message sent from {} to {}: {:?} (transit: {:?}μs)",
                            src,
                            dst,
                            msg_id,
                            transit
                        );
                    }
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    channel.error_count.fetch_add(1, Ordering::Relaxed);
                    self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                    warn!("Channel full, dropping message from {} to {}", src, dst);
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    channel.error_count.fetch_add(1, Ordering::Relaxed);
                    self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                    error!("Channel disconnected for component: {}", dst);
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
// rationale: test-only builder structs assigned field-by-field after
// default(); sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
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
        let (_tx, _rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        let (_dsp_tx, dsp_rx) = bus.create_channel(ComponentId::Dsp).await.unwrap();

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
        let (_tx, _rx) = bus.create_channel(ComponentId::Dsp).await.unwrap();

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
}
