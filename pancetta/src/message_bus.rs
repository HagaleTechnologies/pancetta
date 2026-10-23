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
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, RwLock};
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
    /// Station agent: authenticated remote-control transport (relay + Noise).
    StationAgent,
    /// WSJT-X UDP companion-protocol component (GridTracker/JTAlert interop).
    WsjtxUdp,
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
            ComponentId::StationAgent => write!(f, "station_agent"),
            ComponentId::WsjtxUdp => write!(f, "WsjtxUdp"),
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

/// Severity of a [`MessageType::DiagnosticEvent`]. Re-exported from
/// `pancetta-core` so `pancetta-tui` (which cannot depend on this crate) can
/// share the same type across the bus boundary.
pub use pancetta_core::DiagnosticLevel;

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

    /// A retained, operator-facing diagnostic event (docs/observability-
    /// diagnostics-plan.md Layer 1). Unlike `Error`/`StatusUpdate` (which
    /// exist only to be relayed into the TUI's single, overwrite-prone
    /// status line), this is meant to be RETAINED in a bounded history —
    /// `target` reuses the project's existing `tracing` target vocabulary
    /// (`qso.security`, `tx.policy`, `qso.autonomous`, …) so a diagnostic
    /// panel can filter by the same taxonomy the log file already uses.
    /// Emit sparingly (state changes, drops, rejects, outcomes) — this is
    /// the curated stream, not the tracing firehose.
    DiagnosticEvent {
        target: &'static str,
        level: DiagnosticLevel,
        text: String,
        /// The QSO this event concerns, if any (uppercased qso_id string).
        qso_id: Option<String>,
        /// The DX callsign involved, if any (for at-a-glance filtering).
        callsign: Option<String>,
    },

    /// A structured, retained terminal-QSO outcome (docs/observability-
    /// diagnostics-plan.md Layer 2 — the Recent-QSOs panel). Sibling to
    /// `DiagnosticEvent` above: sent from the SAME `QsoEvent::QsoCompleted`/
    /// `QsoEvent::QsoFailed` handlers (`pancetta/src/coordinator/qso.rs`),
    /// right alongside (not instead of) their existing `DiagnosticEvent`
    /// emission. Also distinct from `QsoHistoryEntry` (#165's flat
    /// success/reason strings for the last-10 strip) — this carries the
    /// richer `QsoOutcome` enum plus a short human-readable timeline so a
    /// future panel can render more than one line per QSO. Emitted once per
    /// QSO reaching a terminal state, never per-decode.
    RecentQsoOutcome(RecentQsoOutcome),

    /// Per-window TX-placement ranking for the TUI instrument
    /// (docs/superpowers/specs/2026-07-03-tui-redesign-design.md §2). Computed
    /// in the autonomous tick from the SAME `SmartFrequencyAllocator` /
    /// `DecodeHistory` / `SpectralSnapshot` the autonomous path allocates
    /// with (`AutonomousOperator::placement_snapshot`) — the TUI never
    /// re-derives scores from a separate computation (single-scorer
    /// invariant). Sent every FT8 window regardless of whether autonomous
    /// mode is enabled (housekeeping, not an action).
    TxPlacementUpdate {
        snapshot: pancetta_qso::frequency::PlacementSnapshot,
    },

    /// Currently-watchlisted callsigns (#197 DX watchlist) — sent every
    /// autonomous tick alongside `TxPlacementUpdate`, same housekeeping
    /// cadence. A full resync each tick (not a diff), matching this
    /// codebase's existing "bulk replace, self-healing" convention (see
    /// `FrequencyAllocator::set_own_frequencies`).
    DxWatchlistUpdate { callsigns: Vec<String> },

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
        /// The station-agent client this request is bound to, iff
        /// `origin == Remote`. The arm gate compares this against the
        /// CURRENTLY-armed client (PAN-91) — a different peer taking control
        /// later must not authorize a frame it never requested. `None` for
        /// every `Local` request.
        remote_client_key_id: Option<String>,
    },

    /// Transmit completed notification
    TransmitComplete {
        success: bool,
        message_text: String,
        duration_ms: u64,
        /// QSO owning this frame, or `None` for untracked manual/tune TX.
        /// Autonomous uses it for PAN-38 self-CQ correlation. PAN-75 mirrors
        /// failed QSO-keyed outcomes as `QsoMessage::TxFrameNotSent` to Qso;
        /// PAN-182 mirrors aired ones as `QsoMessage::TxFrameSent`.
        qso_id: Option<String>,
    },

    /// TX-active indicator for the TUI title-bar badge (Batch 93).
    /// The TX worker sends `active: true` when PTT is asserted and
    /// `active: false` when the transmission ends — via an RAII
    /// observer guard, so abort paths (F8, Shift+Q, shutdown) clear
    /// it just like normal completion. Observation only: this message
    /// never drives PTT or audio.
    TxStatus { active: bool },

    /// A `TxOrigin::Remote` request was dropped by the station-agent arm
    /// gate before keying PTT (dispensa Q-0051 Phase C). Sent alongside the
    /// existing `TxDenied` audit-log append and `DiagnosticEvent` (Phase A/B)
    /// — this one is additionally relayed to `ComponentId::RemoteGateway` so
    /// connected remote clients (panino, cqdx-web) see a live `error` event
    /// instead of silence. `reason` is a short human-readable explanation
    /// (not the raw `ArmState` internals); `qso_id` is `None` for a
    /// multi-TX bundle drop (no single QSO to attribute it to).
    TxDenied {
        reason: String,
        qso_id: Option<String>,
    },

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

    /// Pushed once per keyed TX frame (#172) — Band Activity's own-TX
    /// history. Emitted from `log_tx_frame` at Step 7 (after the slot-
    /// boundary wait, once the actual audio-start instant is known),
    /// separately from — not alongside — `TxQueueStatus`'s NOW-SENDING
    /// push at Step 5; the two are decoupled so this event's `timestamp`
    /// reflects when audio actually started, not when PTT was asserted
    /// (see `log_tx_frame`'s doc comment in `coordinator/tx.rs`). `qso_id:
    /// None` means a CQ/manual frame, matching `TxItem`'s existing
    /// convention — every keyed frame is logged regardless of origin
    /// (#172 scope: all TX, not just QSO-related).
    TxFrameLogged {
        text: String,
        freq_hz: f64,
        qso_id: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
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
    /// (clears split), and on autonomous band-hop (clears split). When a
    /// split change was not delivered to Hamlib, it carries the unchanged
    /// value the band-plan gate still enforces (PAN-209).
    /// `tx_hz == 0` means simplex (chip hidden). Observation only.
    SplitStatus {
        /// Current split TX frequency in Hz, or 0 for simplex.
        tx_hz: u64,
    },

    /// Station-wide operating-mode echo. Sent by the coordinator relay after
    /// a successful `try_switch_operating_mode` (operator Shift+M). Relayed
    /// additively to the remote gateway (`relay_to_gateway`) so already-
    /// connected clients see a live mode change without needing a
    /// reconnect — dispensa Q-0027 / rig-api v1.1. Observation only.
    ModeStatus {
        /// New active operating mode ("FT8"/"FT4"/"FT2").
        mode: String,
    },

    /// Decode-effort preset echo for the TUI's "DECODE: <PRESET> <ms>ms"
    /// title-bar chip (decoder-speed-overhaul Task 15, operator `e`
    /// keybinding). Sent by the coordinator's TUI-relay command handler
    /// after `effort::cycle_decode_effort` advances the preset and
    /// publishes the resulting budget with its decoder config
    /// (`DecodeWindowConfig`). Unlike
    /// `ModeStatus` (which can be refused while a QSO is active) this
    /// cycle never fails — a budget change never invalidates in-flight
    /// decode state (spec §6.2) — so this is always the NEW preset, never
    /// a refusal echo. Observation only — never drives TX.
    DecodeEffortStatus {
        /// New decode-effort preset label ("ECO"/"STANDARD"/"DEEP"/"MAX"/
        /// "AUTO"), from `pancetta_config::DecodeEffort::label`.
        effort: String,
        /// The budget (ms) now published with its decoder config
        /// (`DecodeWindowConfig`) for this preset (`0` = the protocol
        /// ceiling).
        budget_ms: u64,
    },

    /// TX-offset-hold state echo for the TUI's TX-placement park line +
    /// title-bar chip. Every OTHER writer of `tx_offset_hold_hz` (the `o`
    /// modal, Enter-park on the TX-placement instrument) is TUI-initiated,
    /// so the TUI updates its own `App.tx_offset_hold_hz` optimistically
    /// and never needs a round-trip. Task 16's opt-in auto-repark is
    /// coordinator-INITIATED — the autonomous task writes the shared
    /// atomic directly, with no operator keypress to update the TUI's
    /// local copy — so this closes that gap: sent once, right after the
    /// atomic write, so the TUI can re-sync `tx_offset_hold_hz` (and reset
    /// its park-coverage baseline) to the new value. `offset_hz == 0`
    /// means unheld/Auto (same sentinel convention as the atomic itself);
    /// in practice auto-repark only ever sends a nonzero value. Observation
    /// only — never drives TX.
    TxOffsetStatus {
        /// New held TX offset in Hz, or 0 for unheld/Auto.
        offset_hz: u64,
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
        ///
        /// PAN-136: together with `remote_client_key_id` this is the bundle's
        /// gate binding: the fold of its items' bindings
        /// (`coordinator::fold_bundle_identity`), re-folded by the TX worker
        /// after a key-time pivot.
        #[allow(dead_code)]
        origin: TxOrigin,
        /// See `MessageType::TransmitRequest::remote_client_key_id`. The whole
        /// bundle shares one client binding, matching `origin` above: the fold
        /// of its items' bindings, re-folded after a key-time pivot (PAN-136).
        remote_client_key_id: Option<String>,
    },

    /// Audio output samples for transmission. `flush_first`, when true,
    /// discards any samples still buffered from a previous transmission
    /// before these are queued — set for a mid-TX abort/restart re-key
    /// (docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md),
    /// `false` everywhere else (byte-identical to today).
    AudioOutput {
        samples: Vec<f32>,
        sample_rate: u32,
        flush_first: bool,
    },

    /// PAN-135: discard every TX sample still buffered for output, then
    /// settle `ack` once the flush request reached the output ring (the
    /// output callback discards the buffer before it plays another sample).
    /// Travels the same Audio FIFO as `AudioOutput`, so it applies after
    /// every sample queued before it. Sent only by a remote-disarm unwind.
    AudioFlush { sample_rate: u32, ack: DeliveryAck },

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

    /// Pushed once per QSO reaching a terminal state (Completed or Failed) —
    /// #165's last-10-QSOs history panel. Additive; existing
    /// `ActiveQsosSnapshot` handling is untouched.
    QsoHistoryEntry {
        call_sign: String,
        band: String,
        success: bool,
        /// Populated only when `success` is false (e.g. "Timeout").
        reason: Option<String>,
        completed_at: chrono::DateTime<chrono::Utc>,
    },

    /// Waterfall spectrogram data for TUI display
    WaterfallData {
        /// Power values in dB, one row per time step
        power_matrix: Vec<Vec<f32>>,
        /// Frequency range in Hz (min, max)
        freq_range: (f32, f32),
    },

    /// One raw (native-resolution, pre-normalization) waterfall row for the
    /// remote gateway's `spectrum` serverEvent (dispensa Q-0024). Distinct
    /// from `WaterfallData` above (0-1 normalized, TUI-only, never bus-sent):
    /// `mags_db` carries real dB values straight from the decoder, matching
    /// what dispensa's concurred answer described as "natively available
    /// before the 0-1 color-mapping step." `audio_bin_start_hz` is baseband
    /// (0-3000 Hz FT8 passband); the remote gateway adds the current dial
    /// frequency to produce the wire event's RF-absolute `binStartHz`,
    /// mirroring how `DecodedMessage` is enriched with dial frequency in
    /// `remote_gateway::handle_bus_msg`.
    SpectrumRow {
        audio_bin_start_hz: f64,
        bin_width_hz: f64,
        mags_db: Vec<f32>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

/// A single transmit request item for multi-TX bundles.
///
/// PAN-136: each item carries the authorization of the request it came from,
/// so a key-time pivot can swap it atomically with the payload (see
/// `coordinator::pivot_bundle_items`). There is deliberately no `Default`:
/// every construction site must name the binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransmitRequestItem {
    pub message_text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
    /// See `MessageType::TransmitRequest::origin`.
    #[serde(default)]
    pub origin: TxOrigin,
    /// See `MessageType::TransmitRequest::remote_client_key_id`; `None` for
    /// every `Local` item.
    #[serde(default)]
    pub remote_client_key_id: Option<String>,
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

/// How a QSO ended, for `MessageType::RecentQsoOutcome`. Reuses the QSO
/// crate's existing `QsoFailureReason` rather than inventing a parallel
/// taxonomy (`docs/observability-diagnostics-plan.md` Layer 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QsoOutcome {
    /// The QSO reached `QsoState::Completed`.
    Completed,
    /// The QSO reached `QsoState::Failed` — carries WHY.
    Failed(pancetta_qso::QsoFailureReason),
}

/// Payload of `MessageType::RecentQsoOutcome` — a structured, retained
/// record of a single terminal QSO, for the (future) Recent-QSOs panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentQsoOutcome {
    /// The DX station's callsign.
    pub callsign: String,
    /// How the QSO ended.
    pub outcome: QsoOutcome,
    /// Short terminal-state label ("Completed" / "Failed"). NOT the full
    /// prior `QsoState` — `QsoEvent::QsoCompleted`/`QsoFailed` only carry
    /// `QsoMetadata` at this call site, not the state machine's internal
    /// `state_history` (persisting that is Task 4's job, a separate,
    /// larger change).
    pub last_state: String,
    /// Operating frequency in Hz at the time of the outcome.
    pub freq_hz: u32,
    /// When the outcome was recorded.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Short human-readable summary lines built from the QSO's metadata
    /// (start time, exchanged reports, final outcome) — a brief digest, NOT
    /// the full per-message `state_history` timeline (that's Task 4).
    pub brief_timeline: Vec<String>,
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
    /// PAN-142: candidate frequency (Hz) of a pending, not-yet-confirmed
    /// DX-frequency-drift relatch (`QsoMetadata::pending_freq_drift`,
    /// `maybe_confirm_frequency_drift_at`'s two-strike anti-spoof
    /// mechanism). `Some` only while a single off-frequency sighting is
    /// waiting on a second, >=5s-later confirming sighting at the same
    /// frequency before the QSO engine will trust and relatch to it.
    /// `None` when no drift candidate is pending.
    pub pending_freq_drift_hz: Option<f64>,
    /// When the pending drift candidate above was first noted. Paired with
    /// `pending_freq_drift_hz`; always `Some` exactly when that is.
    pub pending_freq_drift_since: Option<chrono::DateTime<chrono::Utc>>,
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
        /// SECURITY: `true` iff this call was initiated by a REMOTE operator
        /// (station agent). The QSO is opened with `remote_origin = true` so
        /// every TX it emits is `TxOrigin::Remote` and armed-TX gated. `false`
        /// for every local/TUI `StartQso` (byte-identical to prior behavior).
        remote_origin: bool,
        /// The station-agent peer (client keyId) that requested this, iff
        /// `remote_origin`. Carried onto the QSO so the arm gate can bind TX
        /// permission to THIS specific client, not just "some client is
        /// armed" — see `QsoMetadata::remote_client_key_id`.
        remote_client_key_id: Option<String>,
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
        /// SECURITY: `true` iff a REMOTE operator answered this caller. Opens
        /// the QSO with `remote_origin = true` (all TX `TxOrigin::Remote`,
        /// armed-TX gated). `false` for local/TUI answers.
        remote_origin: bool,
        /// See `QsoMessage::StartQso::remote_client_key_id`.
        remote_client_key_id: Option<String>,
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
        /// SECURITY: `true` iff a REMOTE operator started this CQ (station
        /// agent `startCq`). The `CallingCq` QSO is opened with
        /// `remote_origin = true` (all TX `TxOrigin::Remote`, armed-TX gated).
        /// `false` for the local/TUI `c` key (byte-identical to prior).
        remote_origin: bool,
        /// See `QsoMessage::StartQso::remote_client_key_id`.
        remote_client_key_id: Option<String>,
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
    /// PAN-137: a station-agent client took control. Retire active QSOs and
    /// queued calls bound to other clients, on all bands. Local and unbound
    /// WSJT-X QSOs stay. Enqueued before the new controller's next QSO intent.
    RemoteControllerChanged { controller_key_id: String },
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
        /// PAN-38: for a self-CQ (`callsign: None`) only, the
        /// `AutonomousOperator`'s `CqStateSnapshot::attempt_id` this
        /// dispatch corresponds to — echoed back on
        /// [`QsoMessage::AutonomousCqDispatchFailed`] if `start_cq` fails, so
        /// the operator can roll back exactly this attempt's speculative
        /// streak/offset mutations. Always `None` for a pounce (no CQ-streak
        /// state to roll back).
        cq_attempt_id: Option<u64>,
    },
    /// PAN-38: `QsoManager::start_cq` failed for a dispatched self-CQ
    /// (`StartAutonomousQso { callsign: None, .. }`) — a downstream
    /// radio/CAT error or subsystem race — after the coordinator's own gates
    /// (Shift+Q, TX policy, operator-presence, dry_run) had all already
    /// permitted the transmission. No QSO was actually opened and nothing
    /// was actually transmitted, so the CQ-no-response streak and any
    /// frequency-switch offset `decide_at` mutated speculatively for this
    /// attempt must be rolled back — same mechanism as a suppressed-before-
    /// dispatch CQ ([`AutonomousOperator::restore_cq_state`]), routed back
    /// through the message bus since the `QsoManager`/`start_cq` call lives
    /// in the QSO component's task, not the autonomous operator's.
    AutonomousCqDispatchFailed {
        /// The `cq_attempt_id` from the `StartAutonomousQso` that failed.
        attempt_id: u64,
    },
    /// PAN-38 round 1 (Codex): `QsoManager::start_cq` SUCCEEDED for a
    /// dispatched self-CQ (a QSO was opened, `qso_id` assigned, and its
    /// opening `MessageToSend` handed to the TX worker) -- but the actual
    /// radio/CAT transmission can still fail downstream, reported later as
    /// `MessageType::TransmitComplete { success: false, .. }`, which carries
    /// no `cq_attempt_id` (it isn't self-CQ-specific machinery). This lets
    /// the autonomous coordinator task record the `qso_id` <-> `attempt_id`
    /// association up front, so it can correlate a later matching
    /// `TransmitComplete` failure back to this attempt and roll it back the
    /// same way a synchronous `start_cq` failure already does.
    AutonomousCqOpened { qso_id: String, attempt_id: u64 },
    /// PAN-75: a failed QSO frame, mirrored by Ft8Transmitter alongside the
    /// Autonomous completion. Qso reverses its remembered keep-call emission
    /// through `QsoManager::roll_back_unsent_attempt`.
    TxFrameNotSent {
        qso_id: String,
        message_text: String,
        /// Set when the frame was replaced by a TX pivot: its own offset and
        /// binding, so the rollback skips the same-text frame that aired.
        replaced: Option<pancetta_qso::UnsentAttemptPayload>,
    },
    /// PAN-182: a QSO frame that aired, mirrored by Ft8Transmitter after
    /// playback alongside the Autonomous completion. Qso uses it to release
    /// a stall offset action held on that frame through
    /// `QsoManager::confirm_stall_trigger_aired`.
    TxFrameSent {
        qso_id: String,
        message_text: String,
        /// The aired frame's own offset and binding, so a pivot-replaced
        /// same-text frame does not release a hold it did not serve.
        aired: pancetta_qso::UnsentAttemptPayload,
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
    /// The operator switched the station-wide operating mode live
    /// (Shift+M, gated by `try_switch_operating_mode` — the coordinator only
    /// sends this after confirming no QSO is active). Updates
    /// `QsoManager::active_mode` for QSOs opened from now on; anything
    /// already logged keeps its already-stamped mode.
    SetOperatingMode {
        /// The new mode string (`"FT8"` / `"FT4"` / `"FT2"`), from
        /// `super::mode_str`.
        mode: String,
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

/// What an enqueue needs from a registered channel (PAN-202).
#[derive(Clone)]
struct ChannelTx {
    sender: Sender<ComponentMessage>,
    message_count: Arc<AtomicU64>,
    error_count: Arc<AtomicU64>,
}

/// Channel pair for component communication
struct ComponentChannel {
    tx: ChannelTx,
    receiver: Receiver<ComponentMessage>,
    component_id: ComponentId,
    last_heartbeat: Arc<RwLock<Option<Instant>>>,
}

/// What a [`DeliveryAckWaiter`] observed (PAN-135).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    /// The consumer applied the request.
    Applied,
    /// The consumer processed the request but could not apply it.
    NotApplied,
    /// Every copy of the ack dropped unsettled: the message was discarded
    /// between sender and consumer (full or closed channel, relay exit).
    Dropped,
    /// Still outstanding when the wait elapsed.
    Pending,
}

/// One-shot "the consumer applied this" confirmation that rides inside a
/// `MessageType` (PAN-135). `MessageType` must stay `Clone`, so the
/// oneshot sender sits in a shared slot: clones share it, the first
/// `complete` wins, and when every clone drops unsettled the waiter sees
/// `Dropped` at once instead of waiting out its timeout.
#[derive(Clone)]
pub struct DeliveryAck {
    slot: Arc<std::sync::Mutex<Option<oneshot::Sender<bool>>>>,
}

impl DeliveryAck {
    /// Create an acknowledgement and the sender's waiter.
    pub fn new() -> (Self, DeliveryAckWaiter) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                slot: Arc::new(std::sync::Mutex::new(Some(tx))),
            },
            DeliveryAckWaiter { rx, settled: None },
        )
    }

    /// Settle the ack. Later calls, on this or any clone, are no-ops.
    pub fn complete(&self, applied: bool) {
        let sender = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(applied);
        }
    }
}

impl std::fmt::Debug for DeliveryAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeliveryAck").finish_non_exhaustive()
    }
}

/// The sender's half of a [`DeliveryAck`].
pub struct DeliveryAckWaiter {
    rx: oneshot::Receiver<bool>,
    settled: Option<AckState>,
}

impl DeliveryAckWaiter {
    /// Wait up to `max` for the ack to settle. `Pending` leaves the waiter
    /// usable; once settled it keeps returning the same state.
    pub async fn wait_for(&mut self, max: Duration) -> AckState {
        if let Some(state) = self.settled {
            return state;
        }
        let state = match tokio::time::timeout(max, &mut self.rx).await {
            Ok(Ok(true)) => AckState::Applied,
            Ok(Ok(false)) => AckState::NotApplied,
            Ok(Err(_)) => AckState::Dropped,
            Err(_) => AckState::Pending,
        };
        if state != AckState::Pending {
            self.settled = Some(state);
        }
        state
    }
}

/// Outcome of [`MessageBus::send_message_if`] and
/// [`MessageBus::send_rig_command_if`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendIfOutcome {
    /// The message was enqueued on the destination's channel.
    Enqueued,
    /// Authorization returned false immediately before the enqueue.
    Denied,
    /// Delivery failed: expired, full, disconnected, or no destination.
    Dropped,
}

/// High-performance message bus for inter-component communication
#[derive(Clone)]
pub struct MessageBus {
    /// Configuration
    config: MessageBusConfig,
    /// Component channels
    channels: Arc<RwLock<HashMap<ComponentId, ComponentChannel>>>,
    /// Hamlib's channel, published by `create_channel` under the table's
    /// write lock (PAN-202). Table entries are never removed or replaced, so
    /// this is always the channel the table holds.
    rig_route: Arc<OnceLock<ChannelTx>>,
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
            rig_route: Arc::new(OnceLock::new()),
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
            rig_route: Arc::new(OnceLock::new()),
            message_counter: Arc::new(AtomicU64::new(0)),
            total_messages: Arc::new(AtomicU64::new(0)),
            dropped_messages: Arc::new(AtomicU64::new(0)),
            expired_messages: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Create a communication channel for a component
    ///
    /// Entries are insert-once: a second registration of the same id errors
    /// here, and nothing removes or replaces an entry. `rig_route` relies on
    /// that, so any future removal or replacement of a channel must also
    /// update `rig_route` in the same critical section (PAN-202).
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
            tx: ChannelTx {
                sender: sender.clone(),
                message_count: Arc::new(AtomicU64::new(0)),
                error_count: Arc::new(AtomicU64::new(0)),
            },
            receiver: receiver.clone(),
            component_id,
            last_heartbeat: Arc::new(RwLock::new(None)),
        };

        if component_id == ComponentId::Hamlib {
            // First and only registration: the duplicate check above returned
            // early otherwise, so this set cannot lose to an earlier one.
            let _ = self.rig_route.set(channel.tx.clone());
        }
        channels.insert(component_id, channel);

        debug!("Created message channel for component: {}", component_id);

        Ok((sender, receiver))
    }

    /// Idempotent variant of `create_channel`: returns the existing registration's
    /// sender/receiver if `component_id` is already registered, instead of erroring.
    /// Needed so a supervisor-restarted component's `start_*_component` (which calls
    /// `create_channel` internally) can be re-invoked without a special restart-only
    /// code path in every component.
    pub async fn get_or_create_channel(
        &self,
        component_id: ComponentId,
    ) -> Result<(Sender<ComponentMessage>, Receiver<ComponentMessage>)> {
        let channels = self.channels.read().await;
        if let Some(existing) = channels.get(&component_id) {
            return Ok((existing.tx.sender.clone(), existing.receiver.clone()));
        }
        drop(channels);
        self.create_channel(component_id).await
    }

    /// Send a message to a specific component, best-effort: every drop
    /// reason (expired, full, disconnected, no destination) is logged and
    /// returned as `Ok(())` (PAN-38). Delivery tiers:
    /// - `send_message`: best-effort.
    /// - `send_message_checked` / `send_message_retrying`: confirmed
    ///   enqueued. Enough for a send that only has to precede later messages
    ///   on the same channel (e.g. PTT-off vs a later PTT-on: Hamlib applies
    ///   its channel in order).
    /// - A [`DeliveryAck`] inside the message: confirmed applied by the
    ///   consumer. Needed when what must follow travels another channel
    ///   (e.g. the disarm audio flush vs the next PTT-on) (PAN-135).
    pub async fn send_message(&self, message: ComponentMessage) -> Result<()> {
        self.send_message_checked(message).await?;
        Ok(())
    }

    /// Same delivery attempt as [`Self::send_message`], but the `bool`
    /// distinguishes an actually-enqueued message (`true`) from every drop
    /// reason `send_message` only logs and swallows into `Ok(())`
    /// (`false`: expired, a full channel, a disconnected receiver, or no
    /// registered channel for the destination).
    ///
    /// PAN-38 round 5 (Codex): `send_message`'s `Result<()>` is NEVER
    /// actually `Err` for a delivery failure — `TrySendError::Full` (the
    /// bounded-queue-drop case) is caught, logged, and mapped to `Ok(())`
    /// just like every other drop reason. A caller checking `if let
    /// Err(e) = ...` for "did this reach the destination" (e.g. the
    /// autonomous self-CQ dispatch, which needs to roll back its
    /// speculative streak/offset mutation if `StartAutonomousQso` never
    /// reaches the QSO task) can never observe a drop through that API —
    /// it always sees success. Callers that only want best-effort,
    /// fire-and-forget delivery (the overwhelming majority of call sites)
    /// should keep using `send_message`; this method is for the rarer
    /// caller that needs to know.
    pub async fn send_message_checked(&self, mut message: ComponentMessage) -> Result<bool> {
        // Check message expiration
        if message.is_expired(self.config.message_timeout_us) {
            self.expired_messages.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Dropping expired message from {} to {} (age: {}μs)",
                message.source,
                message.destination,
                message.age_us()
            );
            return Ok(false);
        }

        // Mark message as sent
        message.latency_tracking.sent_at = Some(Instant::now());

        let channels = self.channels.read().await;

        self.try_send_locked(message, &channels)
    }

    /// Authorize and enqueue without a suspension point between them (PAN-134).
    ///
    /// `authorize` runs inside the table's read lock, after the lock wait and
    /// immediately before enqueue. State changes while waiting behind a writer
    /// are therefore observed before sending. Plain sends retain their existing
    /// best-effort delivery contract.
    ///
    /// The closure MUST be a synchronous leaf: no bus calls, awaits, or blocking
    /// work beyond a short leaf-lock probe such as `remote_tx_permitted_for`.
    /// Lock order is channels, then the closure's locks (ArmState for TX). Never
    /// acquire the bus while holding a lock the closure also takes.
    /// This removes the await gap; it does not hold those leaf locks through
    /// enqueue or prevent another thread from changing state after the probe.
    pub async fn send_message_if(
        &self,
        mut message: ComponentMessage,
        authorize: impl FnOnce() -> bool,
    ) -> Result<SendIfOutcome> {
        if message.is_expired(self.config.message_timeout_us) {
            self.expired_messages.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Dropping expired message from {} to {} (age: {}μs)",
                message.source,
                message.destination,
                message.age_us()
            );
            return Ok(SendIfOutcome::Dropped);
        }
        message.latency_tracking.sent_at = Some(Instant::now());

        let channels = self.channels.read().await;
        if !authorize() {
            debug!(
                "Authorization denied message from {} to {}",
                message.source, message.destination
            );
            return Ok(SendIfOutcome::Denied);
        }
        self.try_send_locked(message, &channels).map(|enqueued| {
            if enqueued {
                SendIfOutcome::Enqueued
            } else {
                SendIfOutcome::Dropped
            }
        })
    }

    /// [`Self::send_message_if`] for a Hamlib command, with no lock and no
    /// await (PAN-202): enqueues on the route `create_channel` published, so
    /// a key-down never waits on a channel registration. Same expiry check,
    /// `authorize` contract (a synchronous leaf, see `send_message_if`) and
    /// accounting as the table path; the closure runs with no bus lock held.
    pub fn send_rig_command_if(
        &self,
        mut message: ComponentMessage,
        authorize: impl FnOnce() -> bool,
    ) -> SendIfOutcome {
        if message.destination != ComponentId::Hamlib {
            error!(
                "send_rig_command_if: destination {} is not Hamlib",
                message.destination
            );
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            return SendIfOutcome::Dropped;
        }
        if message.is_expired(self.config.message_timeout_us) {
            self.expired_messages.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Dropping expired message from {} to {} (age: {}μs)",
                message.source,
                message.destination,
                message.age_us()
            );
            return SendIfOutcome::Dropped;
        }
        message.latency_tracking.sent_at = Some(Instant::now());
        if !authorize() {
            debug!(
                "Authorization denied message from {} to {}",
                message.source, message.destination
            );
            return SendIfOutcome::Denied;
        }
        let Some(route) = self.rig_route.get() else {
            warn!(
                "No channel found for destination component: {}",
                message.destination
            );
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            return SendIfOutcome::Dropped;
        };
        if self.try_send_on(route, message) {
            SendIfOutcome::Enqueued
        } else {
            SendIfOutcome::Dropped
        }
    }

    // Both public delivery APIs use the same synchronous enqueue and accounting.
    fn try_send_locked(
        &self,
        message: ComponentMessage,
        channels: &HashMap<ComponentId, ComponentChannel>,
    ) -> Result<bool> {
        if let Some(channel) = channels.get(&message.destination) {
            Ok(self.try_send_on(&channel.tx, message))
        } else {
            warn!(
                "No channel found for destination component: {}",
                message.destination
            );
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            Ok(false)
        }
    }

    // The enqueue and its accounting, shared by the table path and the rig
    // route (PAN-202).
    fn try_send_on(&self, channel: &ChannelTx, message: ComponentMessage) -> bool {
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
                true
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                channel.error_count.fetch_add(1, Ordering::Relaxed);
                self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                warn!("Channel full, dropping message from {} to {}", src, dst);
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                channel.error_count.fetch_add(1, Ordering::Relaxed);
                self.dropped_messages.fetch_add(1, Ordering::Relaxed);
                error!("Channel disconnected for component: {}", dst);
                false
            }
        }
    }

    /// [`Self::send_message_checked`] with bounded retry, for a send the
    /// caller must not proceed past unconfirmed (PAN-135). `make` builds a
    /// fresh message per attempt (fresh timestamp: a retry is never dropped
    /// as expired). `Ok(true)` once an attempt is enqueued; `Ok(false)` after
    /// `delays.len() + 1` attempts all dropped.
    pub async fn send_message_retrying(
        &self,
        mut make: impl FnMut() -> ComponentMessage,
        delays: &[Duration],
    ) -> Result<bool> {
        for attempt in 0..=delays.len() {
            if self.send_message_checked(make()).await? {
                return Ok(true);
            }
            if let Some(delay) = delays.get(attempt) {
                tokio::time::sleep(*delay).await;
            }
        }
        Ok(false)
    }

    /// Get health status for all components
    pub async fn get_component_health(&self) -> Vec<ComponentHealth> {
        let channels = self.channels.read().await;
        let mut health_status = Vec::new();

        for (&component_id, channel) in channels.iter() {
            let message_count = channel.tx.message_count.load(Ordering::Relaxed);
            let error_count = channel.tx.error_count.load(Ordering::Relaxed) as u32;

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

/// Holds the channel table's write lock, as an in-flight `create_channel`
/// does (PAN-202 tests outside this module).
#[cfg(test)]
pub(crate) struct ChannelTableHold(
    #[allow(dead_code)] tokio::sync::OwnedRwLockWriteGuard<HashMap<ComponentId, ComponentChannel>>,
);

#[cfg(test)]
impl MessageBus {
    pub(crate) async fn hold_channel_table(&self) -> ChannelTableHold {
        ChannelTableHold(self.channels.clone().write_owned().await)
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
    fn ptt_off_msg() -> ComponentMessage {
        ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetPtt { state: false }),
            Instant::now(),
        )
    }
    fn filler(to: ComponentId) -> ComponentMessage {
        ComponentMessage::new(to, to, MessageType::AudioData(Vec::new()), Instant::now())
    }
    const PAN135_DELAYS: [Duration; 3] = [
        Duration::from_millis(5),
        Duration::from_millis(20),
        Duration::from_millis(50),
    ];

    /// Hazard pin: plain send_message keeps the PAN-38 best-effort contract.
    #[tokio::test]
    async fn pan135_send_message_still_swallows_a_full_channel_drop() {
        let bus = MessageBus::new(1).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        assert!(bus
            .send_message_checked(filler(ComponentId::Hamlib))
            .await
            .unwrap());
        assert!(bus.send_message(ptt_off_msg()).await.is_ok());
        assert_eq!(
            rx.try_iter()
                .filter(|m| matches!(m.message_type, MessageType::RigControl(_)))
                .count(),
            0,
            "the PTT-off was dropped, yet send_message reported Ok(())"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan135_send_message_retrying_delivers_once_room_frees() {
        let bus = MessageBus::new(1).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        assert!(bus
            .send_message_checked(filler(ComponentId::Hamlib))
            .await
            .unwrap());
        let rx2 = rx.clone();
        let drainer = tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            rx2.try_recv().expect("filler");
        });
        assert!(bus
            .send_message_retrying(ptt_off_msg, &PAN135_DELAYS)
            .await
            .unwrap());
        drainer.await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap().message_type,
            MessageType::RigControl(RigControlMessage::SetPtt { state: false })
        ));
    }

    #[tokio::test]
    async fn pan135_send_message_retrying_reports_undelivered_after_every_attempt() {
        let bus = MessageBus::new(1).unwrap();
        let (_tx, _rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        assert!(bus
            .send_message_checked(filler(ComponentId::Hamlib))
            .await
            .unwrap());
        let mut built = 0;
        let delivered = bus
            .send_message_retrying(
                || {
                    built += 1;
                    ptt_off_msg()
                },
                &PAN135_DELAYS,
            )
            .await
            .unwrap();
        assert!(!delivered);
        assert_eq!(
            built,
            PAN135_DELAYS.len() + 1,
            "one fresh message per attempt"
        );
    }

    #[tokio::test]
    async fn pan135_ack_dropped_with_a_rejected_message_is_observed_immediately() {
        let bus = MessageBus::new(1).unwrap();
        let (_tx, _rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        assert!(bus
            .send_message_checked(filler(ComponentId::Audio))
            .await
            .unwrap());
        let (ack, mut waiter) = DeliveryAck::new();
        let flush = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioFlush {
                sample_rate: 12_000,
                ack,
            },
            Instant::now(),
        );
        assert!(!bus.send_message_checked(flush).await.unwrap());
        let started = Instant::now();
        assert_eq!(
            waiter.wait_for(Duration::from_secs(2)).await,
            AckState::Dropped
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "no timeout wait"
        );
    }

    #[tokio::test]
    async fn pan135_ack_settles_once_through_any_clone() {
        let (ack, mut waiter) = DeliveryAck::new();
        assert_eq!(
            waiter.wait_for(Duration::from_millis(10)).await,
            AckState::Pending
        );
        let clone = ack.clone();
        clone.complete(true);
        ack.complete(false); // later settlements are no-ops
        assert_eq!(
            waiter.wait_for(Duration::from_millis(10)).await,
            AckState::Applied
        );
        // A settled waiter keeps answering (never polls a finished oneshot).
        assert_eq!(
            waiter.wait_for(Duration::from_millis(10)).await,
            AckState::Applied
        );
    }

    #[tokio::test]
    async fn pan135_ack_reports_not_applied() {
        let (ack, mut waiter) = DeliveryAck::new();
        ack.complete(false);
        assert_eq!(
            waiter.wait_for(Duration::from_millis(10)).await,
            AckState::NotApplied
        );
    }

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
    async fn get_or_create_channel_returns_existing_registration_without_error() {
        let bus = MessageBus::new(1000).unwrap();
        let (tx1, _rx1) = bus.create_channel(ComponentId::Dsp).await.unwrap();
        let (tx2, _rx2) = bus.get_or_create_channel(ComponentId::Dsp).await.unwrap();
        // Same underlying channel: a message sent via tx1 must be visible to a
        // receiver obtained from the second call's rx (same crossbeam sender clone).
        tx1.send(ComponentMessage::new(
            ComponentId::Coordinator,
            ComponentId::Dsp,
            MessageType::Control(ControlMessage::Shutdown),
            Instant::now(),
        ))
        .unwrap();
        drop(tx2); // tx2 is a clone of the same sender; dropping it must not close the channel
                   // A fresh create_channel for the SAME id must still error (proves get_or_create
                   // didn't silently replace the registration under the hood).
        assert!(bus.create_channel(ComponentId::Dsp).await.is_err());
    }

    #[tokio::test]
    async fn get_or_create_channel_creates_when_absent() {
        let bus = MessageBus::new(1000).unwrap();
        let result = bus.get_or_create_channel(ComponentId::Audio).await;
        assert!(result.is_ok());

        // A duplicate create_channel should now fail, proving the registration
        // was actually inserted (not just returned as an ephemeral pair).
        assert!(bus.create_channel(ComponentId::Audio).await.is_err());
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
    #[tokio::test(flavor = "current_thread")]
    async fn pan134_send_message_if_authorizes_after_lock_wait() {
        use std::sync::atomic::AtomicBool;
        let bus = MessageBus::new(16).unwrap();
        let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        // Hold the table's write lock the way an in-flight component
        // restart's create_channel does.
        let w = bus.channels.write().await;
        let denied = std::sync::Arc::new(AtomicBool::new(false));
        let d2 = denied.clone();
        let bus2 = bus.clone();
        let msg = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioData(vec![0.5]),
            Instant::now(),
        );
        let task = tokio::spawn(async move {
            bus2.send_message_if(msg, move || !d2.load(Ordering::SeqCst))
                .await
                .unwrap()
        });
        // Run the spawned sender to its lock-wait Pending point.
        tokio::task::yield_now().await;
        // The disarm lands while the sender is queued behind the writer.
        denied.store(true, Ordering::SeqCst);
        drop(w);
        let outcome = task.await.unwrap();
        assert!(
            matches!(outcome, SendIfOutcome::Denied),
            "mid-wait disarm must deny the enqueue"
        );
        assert!(rx.try_recv().is_err(), "nothing may be enqueued");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pan134_plain_send_still_enqueues_despite_mid_wait_disarm() {
        use std::sync::atomic::AtomicBool;
        let bus = MessageBus::new(16).unwrap();
        let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        let w = bus.channels.write().await;
        let denied = std::sync::Arc::new(AtomicBool::new(false));
        let d2 = denied.clone();
        let bus2 = bus.clone();
        let task = tokio::spawn(async move {
            // Today's tx.rs shape: synchronous check BEFORE the bus send.
            if d2.load(Ordering::SeqCst) {
                return false;
            }
            bus2.send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Audio,
                MessageType::AudioData(vec![0.5]),
                Instant::now(),
            ))
            .await
            .unwrap();
            true
        });
        tokio::task::yield_now().await;
        denied.store(true, Ordering::SeqCst);
        drop(w);
        let enqueued = task.await.unwrap();
        assert!(
            enqueued && rx.try_recv().is_ok(),
            "hazard pinned: plain check-then-send enqueues across a mid-wait disarm"
        );
        // Documents today's hazard for plain send_message; it must KEEP passing
        // after PAN-134 (plain-send semantics are unchanged) — the fix is the
        // send_message_if test above.
    }

    fn ptt_on_msg() -> ComponentMessage {
        ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetPtt { state: true }),
            Instant::now(),
        )
    }

    /// PAN-202: a key-down is enqueued within the cap while a channel
    /// registration holds the table. The writer holds until this test
    /// releases it, so the send runs entirely inside the hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan202_rig_command_enqueues_while_a_registration_holds_the_table() {
        let bus = MessageBus::new(16).unwrap();
        let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let cap = Duration::from_millis(50);
        let (locked_tx, locked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let bus_w = bus.clone();
        let writer = tokio::spawn(async move {
            let _w = bus_w.channels.write().await;
            let _ = locked_tx.send(());
            let _ = release_rx.await;
        });
        locked_rx.await.unwrap();
        let gate_passed_at = Instant::now();
        let outcome = bus.send_rig_command_if(ptt_on_msg(), || true);
        let enqueued_after = gate_passed_at.elapsed();
        assert_eq!(outcome, SendIfOutcome::Enqueued);
        assert!(
            enqueued_after < cap,
            "key-down took {enqueued_after:?}, cap {cap:?}"
        );
        assert!(matches!(
            rx.try_recv().unwrap().message_type,
            MessageType::RigControl(RigControlMessage::SetPtt { state: true })
        ));
        release_tx.send(()).unwrap();
        writer.await.unwrap();
    }

    /// PAN-202 hazard pin: the table path still waits on a registration,
    /// which is why key-down must not use it. Must keep passing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan202_table_send_waits_past_the_cap_on_a_registration() {
        let bus = MessageBus::new(16).unwrap();
        let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let w = bus.channels.write().await;
        let bus2 = bus.clone();
        let send =
            tokio::spawn(async move { bus2.send_message_if(ptt_on_msg(), || true).await.unwrap() });
        sleep(Duration::from_millis(50)).await; // the cap
        assert!(
            !send.is_finished(),
            "the table path enqueued under a held writer"
        );
        assert!(rx.try_recv().is_err());
        drop(w);
        assert_eq!(send.await.unwrap(), SendIfOutcome::Enqueued);
        assert!(rx.try_recv().is_ok());
    }

    /// PAN-202: the route is the registered channel itself — through a
    /// restart's re-registration and from a clone taken before Hamlib
    /// registered — and authorization still gates it under a held table.
    #[tokio::test(flavor = "current_thread")]
    async fn pan202_rig_route_is_the_registered_channel() {
        let bus = MessageBus::new(16).unwrap();
        let early = bus.clone();
        let (_s, rx) = bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap();
        let (_s2, rx2) = bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap();
        let _w = bus.channels.write().await;
        assert_eq!(
            early.send_rig_command_if(ptt_on_msg(), || true),
            SendIfOutcome::Enqueued
        );
        assert!(
            rx2.try_recv().is_ok(),
            "re-registration returns the same queue"
        );
        assert!(rx.try_recv().is_err(), "one message, one queue");
        assert_eq!(
            early.send_rig_command_if(ptt_on_msg(), || false),
            SendIfOutcome::Denied
        );
        assert!(rx.try_recv().is_err(), "a denied key-down enqueues nothing");
    }

    /// PAN-202: same outcomes and counters as `send_message_if`
    /// (mirrors `pan134_conditional_delivery_preserves_drop_accounting`).
    #[tokio::test]
    async fn pan202_rig_command_matches_table_path_accounting() {
        let bus = MessageBus::new(1).unwrap();
        // Hamlib never registered (replay / no-hamlib build): dropped.
        assert_eq!(
            bus.send_rig_command_if(ptt_on_msg(), || true),
            SendIfOutcome::Dropped
        );
        assert_eq!(bus.get_statistics().dropped_messages, 1);
        let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        assert_eq!(
            bus.send_rig_command_if(ptt_on_msg(), || false),
            SendIfOutcome::Denied
        );
        assert_eq!(
            bus.get_statistics().dropped_messages,
            1,
            "denial is not a drop"
        );
        assert_eq!(
            bus.send_rig_command_if(ptt_on_msg(), || true),
            SendIfOutcome::Enqueued
        );
        assert_eq!(
            bus.send_rig_command_if(ptt_on_msg(), || true),
            SendIfOutcome::Dropped
        ); // full
        assert_eq!(bus.get_statistics().total_messages, 1);
        assert_eq!(bus.get_statistics().dropped_messages, 2);
        assert!(rx.try_recv().unwrap().latency_tracking.sent_at.is_some());
        let expired = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetPtt { state: true }),
            Instant::now() - Duration::from_secs(60),
        );
        assert_eq!(
            bus.send_rig_command_if(expired, || panic!("expired messages must not authorize")),
            SendIfOutcome::Dropped
        );
        assert_eq!(bus.get_statistics().expired_messages, 1);
        let not_rig = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioData(vec![0.5]),
            Instant::now(),
        );
        assert_eq!(
            bus.send_rig_command_if(not_rig, || true),
            SendIfOutcome::Dropped
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn pan134_conditional_delivery_preserves_drop_accounting() {
        let bus = MessageBus::new(1).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
        let message = || {
            ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Audio,
                MessageType::AudioData(vec![0.5]),
                Instant::now(),
            )
        };
        assert_eq!(
            bus.send_message_if(message(), || false).await.unwrap(),
            SendIfOutcome::Denied
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(bus.get_statistics().total_messages, 0);
        assert_eq!(bus.get_statistics().dropped_messages, 0);
        assert_eq!(
            bus.send_message_if(message(), || true).await.unwrap(),
            SendIfOutcome::Enqueued
        );
        assert_eq!(
            bus.send_message_if(message(), || true).await.unwrap(),
            SendIfOutcome::Dropped
        );
        assert!(!bus.send_message_checked(message()).await.unwrap());
        assert!(bus.send_message(message()).await.is_ok());
        assert_eq!(bus.get_statistics().total_messages, 1);
        assert_eq!(bus.get_statistics().dropped_messages, 3);
        let received = rx.try_recv().unwrap();
        assert!(received.latency_tracking.sent_at.is_some());
        assert!(rx.try_recv().is_err());

        let absent = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetPtt { state: true }),
            Instant::now(),
        );
        assert_eq!(
            bus.send_message_if(absent, || true).await.unwrap(),
            SendIfOutcome::Dropped
        );
        assert_eq!(bus.get_statistics().dropped_messages, 4);
        let expired = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioData(vec![0.5]),
            Instant::now() - Duration::from_secs(60),
        );
        assert_eq!(
            bus.send_message_if(expired, || panic!("expired messages must not authorize"))
                .await
                .unwrap(),
            SendIfOutcome::Dropped
        );
        assert_eq!(bus.get_statistics().expired_messages, 1);
        assert!(rx.try_recv().is_err());
    }
}
