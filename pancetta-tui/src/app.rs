use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::MouseEvent;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::{Config, Theme};

// Test-only thread-local backing `App::set_test_tui_state_path` /
// `App::tui_state_path`. See those for why this is a thread-local rather
// than a process-global env var. Never compiled into production builds.
#[cfg(test)]
thread_local! {
    static TEST_TUI_STATE_PATH_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Terminal color support level, detected at startup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorCapability {
    /// 256-color (xterm-256color, COLORTERM=256color, etc.)
    TwoFiftySix,
    /// Basic 16-color (most terminals, including SSH defaults)
    Basic,
}

impl ColorCapability {
    pub fn detect() -> Self {
        // COLORTERM=truecolor or 24bit implies 256-color support too
        if let Ok(ct) = std::env::var("COLORTERM") {
            let ct = ct.to_lowercase();
            if ct == "truecolor" || ct == "24bit" || ct == "256color" {
                return Self::TwoFiftySix;
            }
        }
        // Check TERM for 256color suffix
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color") {
                return Self::TwoFiftySix;
            }
        }
        Self::Basic
    }
}

/// View model for decoded messages in the TUI.
/// This is NOT the domain type from pancetta-ft8; it is a display-oriented
/// struct tailored for the UI layer.  If pancetta-ft8 is added as a dependency
/// in the future, add a `From<pancetta_ft8::message::DecodedMessage>` impl
/// instead of duplicating fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedMessageView {
    pub timestamp: DateTime<Utc>,
    pub frequency: f64,
    pub mode: String,
    pub snr: i32,
    pub delta_time: f32,
    pub delta_freq: f32,
    pub call_sign: Option<String>,
    pub grid_square: Option<String>,
    pub message: String,
    pub distance: Option<f64>,
    pub bearing: Option<f64>,
    /// Which 15-second FT8 slot the message was decoded from.
    /// `None` if the source did not tag the message with slot parity (e.g.
    /// test fixtures or legacy code paths).  Used by the TUI Space-press
    /// handler to tell the QSO layer which parity the remote station is
    /// transmitting on so we can reply on the opposite parity.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
    /// `true` if this decode's `to_callsign` matches our station callsign
    /// (case-insensitive, stripping /R or /P suffixes). Computed at the
    /// tui_relay layer where we have access to station config. The Band
    /// Activity panel pins these to the top of the list and styles them
    /// in bold + accent color so the operator can't miss someone calling
    /// them. Defaults to `false` in test fixtures.
    pub is_directed_at_us: bool,
    /// `true` if this station has already been worked **on the current
    /// band**. Computed at the tui_relay layer from the coordinator's
    /// `CachedStationLookup::is_duplicate` — the exact same instance and
    /// method the autonomous priority scorer uses for its duplicate
    /// penalty, so the TUI and the scorer can never disagree. Matching
    /// is uppercase-exact on the full logged callsign (no /P-style
    /// suffix stripping), again because that is what the scorer does.
    /// Defaults to `false` in test fixtures and legacy paths.
    #[serde(default)]
    pub worked_before: bool,
    /// `true` if this station's DXCC entity is still needed (from cqdx.io's
    /// needed set, via the same `CachedStationLookup` the scorer uses).
    /// Inert (always false) when cqdx is unconfigured. Test-default false.
    #[serde(default)]
    pub needed: bool,
    /// `true` if the entity is an ATNO (all-time new one — never worked on
    /// any band). A strict subset of `needed`. Test-default false.
    #[serde(default)]
    pub atno: bool,
    /// Pre-computed display priority score from the coordinator's real
    /// `PriorityScorer` (continuous f64 mapped to [0, 1000]). Set by the
    /// tui-relay thread which has access to `CachedStationLookup` and the
    /// operator's configured `PriorityWeights`. When `Some`, the TUI uses
    /// this directly instead of the coarse bucket function. `None` in test
    /// fixtures and legacy paths.
    #[serde(default)]
    pub priority_score: Option<u32>,
}

/// One in-progress QSO surfaced to the operator. Coordinator-side QSO
/// state machine is the source of truth; this struct is a flattened
/// snapshot pushed to the TUI whenever the state changes (QsoEvent
/// subscription). The banner widget above Band Activity renders all
/// entries compactly so the operator sees who's mid-conversation
/// without leaving the main view.
#[derive(Debug, Clone)]
pub struct ActiveQsoBanner {
    /// Other station's callsign.
    pub their_callsign: String,
    /// Human-readable state ("CqResponse", "Waiting Report", "Sending RR73", etc.).
    pub state: String,
    /// When this QSO started (used to render an elapsed timer).
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Audio frequency in Hz (200-2500 range, where the contra station was
    /// heard / where we're transmitting back).
    pub frequency_hz: f64,
    /// Parity our station transmits in for this QSO. None when unknown.
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Raw text of the last message we transmitted in this QSO (Batch 94:
    /// QSO-detail panel TX line).
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
    /// Stable id of this QSO (UUID string) — target for abort/re-send.
    pub qso_id: String,
    /// How the QSO was initiated: "Manual" or "Auto".
    pub initiated_by: String,
    /// Display-ladder rung labels, left-to-right. Empty when no ladder.
    pub ladder_labels: Vec<String>,
    /// Per-rung flag: `true` if the rung's message is one WE transmit.
    pub ladder_ours: Vec<bool>,
    /// Index of the current rung in `ladder_labels`.
    pub ladder_index: usize,
    /// Human-readable "now" line (what we're doing this moment).
    pub now_line: String,
    /// Human-readable "next" line (what we expect next).
    pub next_line: String,
    /// Manual keep-calling watchdog: calls transmitted so far. `0` when not
    /// keep-calling. Rendered as "Call N/M" so keep-calling reads as bounded.
    pub call_count: u32,
    /// Manual keep-calling watchdog: the call cap. `0` when not keep-calling.
    pub max_calls: u32,
    /// Manual keep-calling watchdog: when keep-calling stops (elapsed-time
    /// bound). `None` when not keep-calling. Rendered as a live countdown.
    pub watchdog_deadline: Option<chrono::DateTime<chrono::Utc>>,
    /// #41: short summary of what the DX is doing on the band (their latest
    /// decoded frame): "CQ", "→ W1XYZ R-12", "→ us -09". `None` when silent.
    pub dx_last_activity: Option<String>,
    /// `true` when this QSO is using the FT8 DXpedition Hound procedure.
    /// Drives a "HOUND" badge in the QSO-status panel. `false` for all
    /// non-Hound QSOs (additive, no change to existing rendering).
    pub hound: bool,
}

/// One entry in the cross-parity manual-call queue, pushed to the TUI as
/// part of every `ActiveQsosUpdate` so the operator sees calls that are
/// waiting — not silently dropped — while the active TX window is committed
/// to the opposite parity (#40).
#[derive(Debug, Clone)]
pub struct PendingCallBanner {
    /// DX callsign the operator chose to work.
    pub callsign: String,
    /// The DX's slot parity. We'd transmit on the opposite. `None` is
    /// theoretically possible but in practice always `Some` for queued calls.
    pub dx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Seconds since the call was parked in the queue.
    pub waited_secs: u64,
}

/// Pipeline component health snapshot, forwarded from coordinator
#[derive(Debug, Clone)]
pub struct PipelineHealth {
    /// Audio thread alive and producing samples
    pub audio_alive: bool,
    /// Number of DSP windows sent to decoder
    pub dsp_windows: u64,
    /// Audio RMS of last DSP window (0.0 = silence)
    pub last_rms: f32,
    /// Whether ft8_lib C decoder is compiled (vs stub)
    pub ft8lib_available: bool,
    /// Total messages decoded this session
    pub total_decodes: u64,
}

/// Per-QSO entry for the QSO-detail panel. Batch 94: populated live
/// from `ActiveQsosUpdate` snapshots (see `App::apply_active_qsos`) —
/// the coordinator's QSO state machine is the source of truth and this
/// is a passive view of it.
#[derive(Debug, Clone, Default)]
pub struct QsoStatus {
    pub active: bool,
    pub call_sign: Option<String>,
    /// Audio frequency in Hz where this QSO is being worked.
    pub frequency: Option<f64>,
    pub mode: Option<String>,
    /// QSO state-machine phase ("wait rpt", "sending RR73", ...).
    pub state: Option<String>,
    /// Their report of our signal (how they hear us) — drives the TX SNR gauge.
    pub snr_tx: Option<i32>,
    /// Measured SNR of their last received message — drives the RX SNR gauge.
    pub snr_rx: Option<i32>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_tx: Option<DateTime<Utc>>,
    pub last_rx: Option<DateTime<Utc>>,
    /// Raw text of the last message we sent in this QSO.
    pub last_tx_text: Option<String>,
    /// Raw text of the last message we received in this QSO.
    pub last_rx_text: Option<String>,
    /// Signal report we sent them.
    pub report_sent: Option<i32>,
    /// Signal report we received from them.
    pub report_received: Option<i32>,
    pub exchange_count: u32,
    /// Stable id of this QSO (UUID string) — target for abort/re-send.
    pub qso_id: Option<String>,
    /// How the QSO was initiated: "Manual" or "Auto".
    pub initiated_by: Option<String>,
    /// Display-ladder rung labels, left-to-right. Empty when no ladder.
    pub ladder_labels: Vec<String>,
    /// Per-rung flag: `true` if the rung's message is one WE transmit.
    pub ladder_ours: Vec<bool>,
    /// Index of the current rung in `ladder_labels`.
    pub ladder_index: usize,
    /// Human-readable "now" line (what we're doing this moment).
    pub now_line: String,
    /// Human-readable "next" line (what we expect next).
    pub next_line: String,
    /// Manual keep-calling watchdog: calls transmitted so far. `0` when not
    /// keep-calling.
    pub call_count: u32,
    /// Manual keep-calling watchdog: the call cap. `0` when not keep-calling.
    pub max_calls: u32,
    /// Manual keep-calling watchdog deadline (elapsed-time bound). `None`
    /// when not keep-calling.
    pub watchdog_deadline: Option<DateTime<Utc>>,
    /// #41: short summary of what the DX is doing on the band (their latest
    /// decoded frame): "CQ", "→ W1XYZ R-12", "→ us -09". `None` when silent.
    pub dx_last_activity: Option<String>,
    /// `true` when this QSO is using the FT8 DXpedition Hound procedure.
    /// Drives the "HOUND" badge in the QSO-status detail panel.
    pub hound: bool,
}

#[derive(Debug, Clone)]
pub struct StationInfo {
    pub call_sign: String,
    pub grid_square: String,
    pub power: u32,
    pub antenna: String,
    pub rig: String,
    pub operating_frequency: f64,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpotSource {
    /// Decoded by our receiver
    Local,
    /// From cqdx.io live spots
    Network,
    /// Seen locally AND in network
    Both,
}

impl std::fmt::Display for SpotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotSource::Local => write!(f, "RX"),
            SpotSource::Network => write!(f, "NET"),
            SpotSource::Both => write!(f, "RX+N"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DxStation {
    pub call_sign: String,
    pub grid_square: Option<String>,
    pub frequency: f64,
    pub mode: String,
    pub last_seen: DateTime<Utc>,
    pub snr: i32,
    pub distance: Option<f64>,
    pub bearing: Option<f64>,
    pub worked_before: bool,
    /// DXCC entity still needed (cqdx needed set). Inert when cqdx is off.
    pub needed: bool,
    /// Entity is an ATNO (all-time new one). Subset of `needed`.
    pub atno: bool,
    pub priority_score: u32,
    // CQDX network metadata
    pub source: SpotSource,
    /// DXCC entity / country name (e.g. "Japan"), from the cqdx live-spot
    /// `dxEntityName`. `Some` for network/Both spots; `None` for local-only
    /// decodes and DX-cluster spots (no entity resolver in the TUI yet).
    pub entity_name: Option<String>,
    pub rarity_tier: Option<String>,
    pub reporter_count: Option<u32>,
    pub is_notable: bool,
    pub notable_type: Option<String>,
    pub confidence: Option<f64>,
    pub best_snr_network: Option<i32>,
    pub last_seen_network: Option<i64>,
    /// Audio offset (Hz, within the FT8 passband) where a LOCAL decode
    /// placed this station. `Some` only for stations seen via a local
    /// decode (`add_decoded_message`); `None` for network-only DX-cluster
    /// spots, which carry only the dial frequency. The Space call-target
    /// uses this so we reply on the operator's-heard offset for local
    /// decodes instead of hard-coding 1500 Hz.
    pub audio_offset_hz: Option<u64>,
    /// Which 15-second slot a LOCAL decode placed this station on. `None`
    /// for network spots (no slot information). Threaded into the
    /// `CallStation` command so the QSO layer can reply on the opposite
    /// parity.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
}

/// Status data received from the autonomous operator.
#[derive(Debug, Clone, Default)]
pub struct AutonomousStatus {
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DevicePanel {
    Input,
    Output,
}

/// One retained diagnostic event (docs/observability-diagnostics-plan.md
/// Layer 1) — relayed from `MessageType::DiagnosticEvent`. `target` reuses
/// the project's existing `tracing` target vocabulary so the overlay reads
/// like a curated slice of the log, not a new ad-hoc taxonomy.
#[derive(Debug, Clone)]
pub struct DiagnosticEventRecord {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub target: &'static str,
    pub level: pancetta_core::DiagnosticLevel,
    pub text: String,
    pub qso_id: Option<String>,
    pub callsign: Option<String>,
}

/// One scored candidate slice in the TX-placement instrument, mirroring
/// `pancetta_qso::frequency::FrequencyCandidate` field-for-field (minus
/// `clear_both_slots`/`noise_floor`, which the instrument doesn't need).
/// Deliberately a TUI-local type — `pancetta-tui` must not depend on
/// `pancetta-qso`; the coordinator's relay layer (`tui_relay.rs`) converts.
#[derive(Debug, Clone)]
pub struct PlacementSlice {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_first: bool,
    pub clear_second: bool,
}

/// A ranked snapshot of band openness for the TX-placement instrument,
/// mirroring `pancetta_qso::frequency::PlacementSnapshot`. TUI-local type
/// (see `PlacementSlice`); relayed from `MessageType::TxPlacementUpdate` via
/// `TuiMessage::TxPlacementUpdate`.
#[derive(Debug, Clone)]
pub struct PlacementView {
    /// Top-N ranked candidates, sorted by score descending.
    pub slices: Vec<PlacementSlice>,
    /// Per-bin openness code across the full allocation range:
    /// 0=busy-both, 1=second-only-clear, 2=first-only-clear, 3=clear-both.
    pub openness: Vec<u8>,
    /// Bin width in Hz (matches the allocator's `step_hz`).
    pub bin_hz: f64,
    /// Allocation range (min, max) in Hz.
    pub range: (f64, f64),
    /// When this snapshot was received by the TUI.
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// Which field of the frequency-entry modal is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreqModalField {
    /// RX dial input field.
    #[default]
    RxDial,
    /// TX split input field (blank = simplex).
    TxSplit,
}

/// State for the Shift+F frequency-entry modal: two MHz text fields.
#[derive(Debug, Clone, Default)]
pub struct FreqModalState {
    /// Modal visible.
    pub visible: bool,
    /// RX dial input buffer (MHz string).
    pub rx_buffer: String,
    /// TX split input buffer (MHz string); empty = simplex.
    pub tx_buffer: String,
    /// Focused field.
    pub field: FreqModalField,
}

/// State for the `o`-key TX-audio-offset modal: a single integer Hz field.
///
/// Valid range: 300–2700 Hz (matching `pancetta_qso::TX_OFFSET_{MIN,MAX}_HZ`).
/// Blank entry on Enter clears the held offset (→ Auto). Out-of-range input is
/// rejected with a status message.
#[derive(Debug, Clone, Default)]
pub struct OffsetModalState {
    /// Modal visible.
    pub visible: bool,
    /// Hz input buffer (decimal digits only).
    pub buffer: String,
}

/// Parse an integer Hz value from a decimal string. Returns `None` for empty,
/// non-numeric, or negative input.
pub fn parse_hz(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<u64>().ok()
}

/// Valid TX audio offset range (inclusive), in Hz.
///
/// Must match `pancetta_qso::TX_OFFSET_MIN_HZ` / `TX_OFFSET_MAX_HZ`
/// (300.0 / 2700.0) — the coordinator clamps to those bounds via
/// `deconflict_offset`, so accepting values outside them would cause the
/// status chip to display an offset that is silently clamped on use.
pub const TX_OFFSET_MIN_HZ: u64 = 300;
/// Valid TX audio offset range (inclusive), in Hz.
///
/// Must match `pancetta_qso::TX_OFFSET_MAX_HZ` (2700.0). See
/// [`TX_OFFSET_MIN_HZ`] for the rationale.
pub const TX_OFFSET_MAX_HZ: u64 = 2700;

#[derive(Debug, Clone)]
pub struct DeviceSelectionState {
    pub input_devices: Vec<(String, bool)>, // (name, is_default)
    pub output_devices: Vec<(String, bool)>,
    pub selected_input_idx: usize,
    pub selected_output_idx: usize,
    pub active_panel: DevicePanel,
    pub visible: bool,
}

impl Default for DeviceSelectionState {
    fn default() -> Self {
        Self {
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            selected_input_idx: 0,
            selected_output_idx: 0,
            active_panel: DevicePanel::Input,
            visible: false,
        }
    }
}

impl DeviceSelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the currently selected index for the active panel.
    pub fn selected_idx(&self) -> usize {
        match self.active_panel {
            DevicePanel::Input => self.selected_input_idx,
            DevicePanel::Output => self.selected_output_idx,
        }
    }

    /// Move selection up in the active panel.
    pub fn move_up(&mut self) {
        match self.active_panel {
            DevicePanel::Input => {
                if self.selected_input_idx > 0 {
                    self.selected_input_idx -= 1;
                }
            }
            DevicePanel::Output => {
                if self.selected_output_idx > 0 {
                    self.selected_output_idx -= 1;
                }
            }
        }
    }

    /// Move selection down in the active panel.
    pub fn move_down(&mut self) {
        match self.active_panel {
            DevicePanel::Input => {
                let max = self.input_devices.len().saturating_sub(1);
                if self.selected_input_idx < max {
                    self.selected_input_idx += 1;
                }
            }
            DevicePanel::Output => {
                let max = self.output_devices.len().saturating_sub(1);
                if self.selected_output_idx < max {
                    self.selected_output_idx += 1;
                }
            }
        }
    }

    /// Toggle between Input and Output panels.
    pub fn toggle_panel(&mut self) {
        self.active_panel = match self.active_panel {
            DevicePanel::Input => DevicePanel::Output,
            DevicePanel::Output => DevicePanel::Input,
        };
    }

    /// Get the selected input device name.
    pub fn selected_input_name(&self) -> Option<String> {
        self.input_devices
            .get(self.selected_input_idx)
            .map(|(name, _)| name.clone())
    }

    /// Get the selected output device name.
    pub fn selected_output_name(&self) -> Option<String> {
        self.output_devices
            .get(self.selected_output_idx)
            .map(|(name, _)| name.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActivePanel {
    BandActivity,
    QsoStatus,
    /// Stations currently calling US (directed-at-us decodes). Operator picks
    /// one and replies at the correct sequence step (smart default + override).
    Callers,
    DxHunter,
    /// The TX-placement instrument. Reachable via `5` (Task 18 renumbered the
    /// panel-jump keys after Station Info was demoted) as well as
    /// `Tab`/`Shift+Tab`. Has no "focused callsign" concept (see
    /// `focused_callsign`).
    TxPlacement,
}

impl ActivePanel {
    // Tab cycle follows the on-screen layout top-to-bottom, left column then
    // right column: BandActivity, QsoStatus (left) → DxHunter, Callers
    // (right). The full-width-waterfall layout put Callers BELOW DX Hunter,
    // so DxHunter precedes Callers here (the enum's declaration order is
    // unrelated and left unchanged). TxPlacement (Task 11) is inserted right
    // after BandActivity — it sits directly below the active-QSO banner and
    // above the rest of the grid on-screen in Operate. `StationInfo` was
    // removed from the cycle in Task 18 (demoted to a non-navigable station
    // card + status-bar spans — it no longer has an `ActivePanel` variant).
    pub fn next(&self) -> Self {
        match self {
            ActivePanel::BandActivity => ActivePanel::TxPlacement,
            ActivePanel::TxPlacement => ActivePanel::QsoStatus,
            ActivePanel::QsoStatus => ActivePanel::Callers,
            ActivePanel::Callers => ActivePanel::DxHunter,
            ActivePanel::DxHunter => ActivePanel::BandActivity,
        }
    }

    pub fn previous(&self) -> Self {
        match self {
            ActivePanel::BandActivity => ActivePanel::DxHunter,
            ActivePanel::DxHunter => ActivePanel::Callers,
            ActivePanel::Callers => ActivePanel::QsoStatus,
            ActivePanel::QsoStatus => ActivePanel::TxPlacement,
            ActivePanel::TxPlacement => ActivePanel::BandActivity,
        }
    }
}

/// What the Space key should DO for the currently-selected station — resolved
/// by [`App::resolve_space_action`]. Space means "do the right next thing":
///
/// - [`SpaceAction::Reply`] when the selected callsign most-recently sent us a
///   message directed at us (a grid/report/R-report/RR73/73). We reply at the
///   smart-default sequence step (same `classify_caller_reply` logic the
///   Callers panel uses), e.g. their `K5ARH VB7F RR73` → we send `VB7F K5ARH
///   73`. This is what fixes "I clicked to send 73 but it sent my grid".
/// - [`SpaceAction::Call`] when nothing directed-at-us is on record for the
///   callsign (a pure CQer, or a DX-cluster spot we've never heard call us).
///   This is the historical Space behavior — answer their CQ with our grid.
#[derive(Debug, Clone, PartialEq)]
pub enum SpaceAction {
    /// Answer a CQ / start a fresh contact at the grid step. Carries the
    /// fields the coordinator's `CallStation` command needs.
    Call {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Reply to a station that last sent us something directed at us, at the
    /// smart-default sequence step. Carries the fields the coordinator's
    /// `RespondToCaller` command needs.
    Reply {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        snr: Option<f32>,
    },
}

/// A per-band snapshot stashed on band switch and restored when the operator
/// returns to that band within the cache TTL. Holds the decoded-message list
/// (the source of both the Band Activity and Callers panels) and the DX-Hunter
/// station map, plus when it was captured.
#[derive(Debug, Clone)]
struct BandSnapshot {
    decoded_messages: VecDeque<DecodedMessageView>,
    dx_stations: HashMap<String, DxStation>,
    captured_at: DateTime<Utc>,
}

/// A compact, display-oriented TX item for the NOW-SENDING / QUEUED view.
/// Mirrors the coordinator's `message_bus::TxItem` but lives in the TUI so
/// the TUI doesn't depend on the main `pancetta` crate.
#[derive(Debug, Clone)]
pub struct TxQueueItem {
    /// FT8 message text being / to-be transmitted.
    pub text: String,
    /// Absolute audio frequency (Hz).
    pub freq_hz: f64,
    /// QSO id this item belongs to, if any.
    pub qso_id: Option<String>,
    /// When `true`, this item could not be sent in the current slot and was
    /// deferred to a later slot (the WSJT-X-style late-TX 30s defer). The
    /// strip renders it as "deferred" instead of looking dead.
    pub deferred: bool,
}

/// Rig connection state shown as a station-panel badge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RigConnDisplay {
    /// No CAT link (mock rig / rig control disabled / connect failed).
    #[default]
    NotConnected,
    /// Connected to the rig and polling normally.
    Connected,
    /// Was connected but polls are now failing.
    PollingFailed,
}

pub struct App {
    pub config: Config,
    pub should_quit: bool,
    pub active_panel: ActivePanel,

    // UI State
    pub terminal_size: (u16, u16),
    pub status_message: String,
    pub theme: Theme,
    /// Retained diagnostic-event history (docs/observability-diagnostics-
    /// plan.md Layer 1) — unlike `status_message` (a single, overwrite-
    /// prone line), this is a bounded scrollback of WHY things happened:
    /// QSO outcomes, drops, rejects. Toggled into view alongside the
    /// existing panels; see `ActivePanel`.
    pub diagnostic_events: VecDeque<DiagnosticEventRecord>,
    /// Shift+D overlay visibility for `diagnostic_events`.
    pub show_diagnostics: bool,
    /// Scroll cursor into `diagnostic_events` while the overlay is open.
    pub diagnostics_scroll: usize,
    /// Count of QSOs completed this session (Task 20d). Counted TUI-side
    /// from the diagnostic-event stream that already flows — no new bus
    /// message — by matching the exact completion text the coordinator
    /// emits (`"QSO with {call} logged (RST …/…)"`, target "qso", Info
    /// level). Rendered as "QSOs: {n}" in the title bar once n > 0.
    pub session_completed: u32,
    /// Set by the first `x` press (Task 20f — confirm-on-`x`); a second
    /// press within `CLEAR_CONFIRM_WINDOW` actually clears decodes. `None`
    /// = not armed.
    pub clear_armed_at: Option<std::time::Instant>,

    // Data
    pub decoded_messages: VecDeque<DecodedMessageView>,
    /// Active QSOs (supports multiple concurrent QSOs).
    pub qso_statuses: Vec<QsoStatus>,
    /// Snapshot of in-progress QSOs pushed by the coordinator on every
    /// QSO state change. Rendered in a 1-row banner above Band Activity
    /// so operators see who they're mid-conversation with at all times.
    pub active_qsos: Vec<ActiveQsoBanner>,
    /// Cross-parity manual calls waiting in the queue (#40). Pushed alongside
    /// `active_qsos` in every `ActiveQsosUpdate`. The QSO Status panel renders
    /// these as a "Queued" section so the operator knows a call is waiting
    /// rather than silently dropped.
    pub pending_calls: Vec<PendingCallBanner>,
    /// Selection cursor into the active-QSO set (and the QSO Status panel),
    /// driving which QSO the abort/re-send management keys target. Clamped
    /// to the active set whenever it changes.
    pub qso_cursor: usize,
    /// The `qso_id` the QSO cursor is pinned to (Batch 2 #5). The active-QSO
    /// list grows/shrinks and (pre-Batch-2) re-ordered between snapshots, so a
    /// bare positional cursor silently retargeted `k`/`r` onto a different QSO.
    /// We pin the SELECTED QSO's id and re-derive `qso_cursor` from it on every
    /// snapshot (`clamp_qso_selection`); cursor moves re-pin. `None` = not yet
    /// pinned (track whatever is at `qso_cursor`).
    qso_pinned_id: Option<String>,
    pub station_info: StationInfo,
    pub dx_stations: HashMap<String, DxStation>,
    pub band_activity_scroll: usize,
    /// The callsign the Band Activity cursor is pinned to. The list reorders
    /// on EVERY decode window (directed-at-us pinned first, then newest-first),
    /// so a bare positional index silently retargets Space onto a different
    /// station. Same pattern as `dx_hunter_pinned_call` / `qso_pinned_id`.
    /// `None` = not yet pinned (track whatever is at `band_activity_scroll`).
    band_activity_pinned_call: Option<String>,
    pub dx_hunter_scroll: usize,
    /// The callsign the DX-Hunter cursor is pinned to. The DX-Hunter list
    /// re-sorts and grows under the cursor (needed-first / priority-desc),
    /// so a bare positional index silently retargets Space/Enter onto a
    /// different station when a higher-priority spot arrives. We pin the
    /// SELECTED callsign and re-derive `dx_hunter_scroll` from it after
    /// every list mutation (`clamp_dx_hunter_selection`), falling back to a
    /// clamp if the pinned call left the list. `None` means "not yet pinned"
    /// (track whatever is at `dx_hunter_scroll`).
    dx_hunter_pinned_call: Option<String>,
    /// Selection cursor into the Callers panel (the list of stations calling
    /// us). Indexes `App::displayed_callers()`; clamped when that list changes.
    pub callers_scroll: usize,
    /// The callsign the Callers cursor is pinned to. The Callers list is
    /// newest-first and grows as new callers arrive at row 0, which would
    /// otherwise shift the operator's selection (and the Enter reply-target)
    /// onto whoever just called. We pin the SELECTED caller's callsign and
    /// re-derive `callers_scroll` from it on every list change
    /// (`clamp_callers_selection`). `None` means "not yet pinned".
    callers_pinned_call: Option<String>,
    /// Operator override for the reply sequence step in the Callers panel.
    /// `None` means "use the smart default classified from the selected
    /// caller's last message". Reset to `None` whenever the selected caller
    /// changes, so each freshly-selected caller starts at its own smart
    /// default. Left/Right (when the Callers panel is focused) cycle this.
    pub caller_reply_override: Option<pancetta_core::ResponseStep>,
    /// The callsign the `caller_reply_override` applies to. Used to detect a
    /// selection change and reset the override.
    caller_override_for: Option<String>,

    // Audio processing
    pub audio_device: Option<String>,
    pub is_monitoring: bool,
    pub audio_level: f32,
    /// Last rig S-meter reading, in hamlib STRENGTH convention: dB
    /// relative to S9 (0 = S9, -54 ≈ S0, +20 = S9+20). `None` until the
    /// first reading arrives (no rig, or rig doesn't report STRENGTH).
    pub signal_strength_db: Option<i32>,
    /// Last SWR reading (e.g. 1.3) from the rig while keyed, and when it
    /// arrived. Shown in the status bar only during TX; goes stale after 5s.
    pub swr: Option<f32>,
    pub swr_at: Option<DateTime<Utc>>,
    /// When `signal_strength_db` was last updated. Used to render the
    /// S-meter as stale ("---") if the rig stops reporting.
    pub signal_strength_at: Option<DateTime<Utc>>,
    pub pipeline_health: Option<PipelineHealth>,
    /// `true` when autonomous mode is enabled but the operator has gone idle
    /// past the presence window, so the coordinator is suppressing autonomous
    /// *initiation* (FCC §97.221). Set each frame by the TUI run loop; drives a
    /// title-bar prompt telling the operator to press a key to resume CQ/pounce.
    pub autonomous_init_paused: bool,
    pub color_capability: ColorCapability,
    pub waterfall_data: Vec<Vec<f32>>,

    // Autonomous operator
    pub autonomous_status: Option<AutonomousStatus>,

    // Device selection modal
    pub device_selection: DeviceSelectionState,

    // Help overlay
    pub help_visible: bool,

    /// True while the operator-confirm-quit modal is visible. `q` opens
    /// it; `y`/`Enter` confirms (sends `TuiCommand::Quit`); `n`/`Esc`/`q`
    /// dismisses. Modal blocks all other keys while visible.
    pub quit_confirm_visible: bool,

    /// Frequency-entry modal (Shift+F). See `FreqModalState`.
    pub freq_modal: FreqModalState,
    /// TX-audio-offset modal (`o` key). See `OffsetModalState`.
    pub offset_modal: OffsetModalState,
    /// Held TX audio offset in Hz, or `None` when in Auto mode. Set
    /// optimistically on the TUI side when the operator applies the offset
    /// modal; the coordinator echo is authoritative.
    pub tx_offset_hold_hz: Option<u64>,
    /// Active split TX dial in Hz for display (0 = simplex). Set optimistically
    /// when the operator applies split; the authoritative atomic lives in the
    /// coordinator.
    pub split_tx_hz: u64,
    /// True once this session has shown the out-of-band acknowledgment modal,
    /// so it is shown at most once per session.
    pub out_of_band_warned: bool,
    /// True while the required out-of-band acknowledgment modal is visible.
    pub out_of_band_ack_visible: bool,
    /// The TX RF (Hz) that triggered the out-of-band modal (for its message).
    pub out_of_band_rf_hz: u64,

    /// hb-161 — Phase 5 emergency-stop banner state. Flipped to `true`
    /// the moment the operator presses Shift+Q; the UI renders a red
    /// "STOPPED BY OPERATOR" banner over the main view. Cleared by Esc.
    /// The actual side effects (TX abort, autonomous-off, CQ-stop) are
    /// driven by the `TuiCommand::OperatorEmergencyStop` event the
    /// coordinator handles — this field is just the visual signal so
    /// the operator sees the keypress was received even if the
    /// command-forwarding loop is briefly stalled.
    pub stopped_by_operator: bool,

    /// Fox (DXpedition operator) mode. Flipped optimistically on `Shift+X`
    /// for immediate chip feedback; the coordinator's `SetFoxMode` handler
    /// is the authoritative state machine (starts repeating CQ, raises the
    /// concurrent caller-answer cap to `fox_max_streams`). Defaults to
    /// `false` (Fox mode off).
    pub fox_mode: bool,

    // TX input
    pub tx_input_buffer: String,
    pub tx_input_cursor: usize,
    /// `true` while the operator is composing a free-text TX message (entered
    /// with `/`). In compose mode every `Char`/`Backspace` edits
    /// `tx_input_buffer` instead of triggering a command, Enter sends + exits,
    /// and Esc cancels + exits. Outside compose mode, letters are commands
    /// only — stray keystrokes never feed the TX buffer (the historic bug:
    /// command keys and free-text input shared the same flat keymap).
    pub compose_mode: bool,
    pub is_transmitting: bool,
    pub tx_frequency_offset: f64,

    /// Global tri-state TX policy, mirrored from the coordinator's
    /// `TxPolicyUpdate` echo (and flipped optimistically on the `g` /
    /// Shift+Q keys for instant feedback). Drives the bold, color-coded
    /// TX-policy banner. Defaults to `Full`.
    pub tx_policy: pancetta_core::TxPolicy,
    /// Operator TX-frequency mode (Hold/Auto), flipped optimistically on `f`
    /// for instant chip feedback; the coordinator flips the authoritative
    /// shared atomic. Hold (default) keeps the operator's picked offset sticky.
    pub tx_freq_mode: pancetta_core::TxFreqMode,
    /// The message currently being transmitted (NOW-SENDING), if any.
    /// Updated from the coordinator's `TxQueueUpdate`.
    pub tx_now_sending: Option<TxQueueItem>,
    /// Items queued for an upcoming slot but not yet on the air.
    pub tx_queued: Vec<TxQueueItem>,

    /// Rig connection state for the station-panel badge, mirrored from the
    /// coordinator's `RigStatusUpdate`. Defaults to `NotConnected`.
    pub rig_connected: RigConnDisplay,
    /// `true` when TX audio is routed to the system default output rather than
    /// an explicit rig CODEC (the "PTT keys, audio on speakers" misconfig).
    /// Drives a persistent station-panel warning badge. Mirrored from the
    /// coordinator's `AudioOutputDefault` message.
    pub tx_output_default: bool,

    // Band/frequency tracking
    pub current_band_index: usize,
    /// Frequency reported by the radio (via hamlib), if known. In MHz.
    pub radio_frequency: Option<f64>,
    /// Per-band snapshot cache. On every band switch we stash the band we are
    /// leaving (its decoded messages — the source of BOTH the Band Activity and
    /// Callers lists — and its DX-Hunter stations) keyed by band name with a
    /// timestamp, then clear the live lists. Switching BACK to a band within
    /// [`Self::BAND_CACHE_TTL_SECS`] restores that snapshot so the operator does
    /// not lose who was calling them; an older snapshot is dropped (start
    /// fresh). Bounded to [`Self::BAND_CACHE_MAX`] bands and pruned of stale
    /// entries on each switch so it can't grow without bound.
    band_cache: HashMap<String, BandSnapshot>,

    /// The operator's selected activity view (Phase 2 TUI redesign). `v`/`V`
    /// cycle it; persisted to `~/.pancetta/tui_state.json` so it survives a
    /// restart. `Operate` (the default) renders today's layout unchanged;
    /// later tasks (5-7) make Hunt/Run/Monitor render differently.
    pub active_view: crate::view::ActiveView,

    /// When `true`, `z` has zoomed the focused panel (`active_panel`) to
    /// fill the whole content area, bypassing the active view's grid
    /// layout entirely. Orthogonal to `active_view` — zoom works the same
    /// regardless of which of the 4 views is selected. `z` toggles it;
    /// Esc or Tab also clear it. Never persisted (session-only, like
    /// `help_visible`).
    pub zoomed: bool,

    /// Latest TX-placement instrument snapshot (relayed from
    /// `MessageType::TxPlacementUpdate` via `apply_placement`). `None` until
    /// the first snapshot arrives. Tasks 11-16 render/interact with this.
    pub placement: Option<PlacementView>,
    /// Selection cursor into `placement.slices`. Clamped to
    /// `slices.len().saturating_sub(1)` on every `apply_placement` call so a
    /// shorter new snapshot never leaves the cursor pointing past the end.
    pub placement_cursor: usize,
    /// When the operator "parked" a TX-placement pick, if any. `None` = not
    /// parked. Set/cleared by later tasks (12).
    pub parked_since: Option<DateTime<Utc>>,
    /// `(parked frequency Hz, openness code 0-3 at that frequency's bin)`
    /// as of the LAST `apply_placement` call — the edge-detect baseline for
    /// the one-shot parked-slice degradation warning (Task 14). The
    /// frequency and code are bundled in one tuple so they can never drift
    /// apart: `apply_placement` always updates/resets BOTH together, never
    /// just the code. This matters because the operator can re-park to a
    /// DIFFERENT frequency (Enter-park or the `o` modal, both in
    /// `tui_runner.rs`) faster than a `PlacementView` snapshot arrives
    /// (~every 15s FT8 window) — without the frequency half, the next
    /// `apply_placement` call would compare the NEW frequency's fresh code
    /// against the OLD frequency's stale code and could fire a spurious (or
    /// miss a real) warning. `None` whenever no park is active
    /// (`tx_offset_hold_hz.is_none()`), so unparking/re-parking always
    /// starts a fresh baseline (no stale comparison against a frequency
    /// that's no longer held). Updated on every `apply_placement` call
    /// regardless of whether the warning fired.
    pub park_coverage_last: Option<(u64, u8)>,
}

/// Peak intensity in the latest waterfall row within ±radius_hz of center_hz.
fn spectral_peak(row: &[f32], center_hz: f64, radius_hz: f64, range: (f64, f64)) -> f32 {
    if row.is_empty() {
        return 0.0;
    }
    let (lo, hi) = range;
    let width = row.len() as f64;
    let bin_hz = (hi - lo) / width;
    if bin_hz <= 0.0 {
        return 0.0;
    }
    let center_bin = ((center_hz - lo) / bin_hz) as isize;
    let radius_bins = (radius_hz / bin_hz).ceil() as isize;
    let lo_bin = center_bin.saturating_sub(radius_bins).max(0) as usize;
    let hi_bin = (center_bin + radius_bins).max(0) as usize;
    let hi_bin = hi_bin.min(row.len() - 1);
    if lo_bin > hi_bin {
        return 0.0;
    }
    row[lo_bin..=hi_bin].iter().copied().fold(0.0f32, f32::max)
}

impl App {
    pub async fn new(config: Config, audio_device: Option<String>) -> Result<Self> {
        let station_info = StationInfo {
            call_sign: config.station.call_sign.clone(),
            grid_square: config.station.grid_square.clone(),
            power: config.station.power,
            antenna: config.station.antenna.clone(),
            rig: config.station.rig.clone(),
            operating_frequency: config.station.default_frequency,
            // Station-wide active operating mode (FT8/FT4/FT2) from [rig].mode
            // (carried on StationConfig); backs the title-bar mode chip
            // (rendered only when != FT8).
            mode: config.station.mode.clone(),
        };

        // Find the band index matching the default frequency
        let default_band_index = config
            .bands
            .bands
            .iter()
            .position(|b| {
                station_info.operating_frequency >= b.frequency_range.0
                    && station_info.operating_frequency <= b.frequency_range.1
            })
            .unwrap_or(5); // fallback to 20m

        let mut app = Self {
            config: config.clone(),
            should_quit: false,
            active_panel: ActivePanel::BandActivity,
            terminal_size: (80, 24),
            status_message: "Pancetta TUI Ready".to_string(),
            theme: config.ui.theme,
            diagnostic_events: VecDeque::with_capacity(500),
            show_diagnostics: false,
            diagnostics_scroll: 0,
            session_completed: 0,
            clear_armed_at: None,
            decoded_messages: VecDeque::with_capacity(1000),
            qso_statuses: Vec::new(),
            active_qsos: Vec::new(),
            pending_calls: Vec::new(),
            qso_cursor: 0,
            qso_pinned_id: None,
            station_info,
            dx_stations: HashMap::new(),
            band_activity_scroll: 0,
            band_activity_pinned_call: None,
            dx_hunter_scroll: 0,
            dx_hunter_pinned_call: None,
            callers_scroll: 0,
            callers_pinned_call: None,
            caller_reply_override: None,
            caller_override_for: None,
            audio_device,
            is_monitoring: false,
            audio_level: 0.0,
            signal_strength_db: None,
            signal_strength_at: None,
            swr: None,
            swr_at: None,
            pipeline_health: None,
            autonomous_init_paused: false,
            color_capability: ColorCapability::detect(),
            waterfall_data: Vec::new(),
            autonomous_status: None,
            device_selection: DeviceSelectionState::new(),
            help_visible: false,
            quit_confirm_visible: false,
            freq_modal: FreqModalState::default(),
            offset_modal: OffsetModalState::default(),
            tx_offset_hold_hz: None,
            split_tx_hz: 0,
            out_of_band_warned: false,
            out_of_band_ack_visible: false,
            out_of_band_rf_hz: 0,
            stopped_by_operator: false,
            fox_mode: false,
            tx_input_buffer: String::new(),
            tx_input_cursor: 0,
            compose_mode: false,
            is_transmitting: false,
            tx_frequency_offset: 1500.0,
            tx_policy: pancetta_core::TxPolicy::default(),
            tx_freq_mode: pancetta_core::TxFreqMode::default(),
            tx_now_sending: None,
            tx_queued: Vec::new(),
            rig_connected: RigConnDisplay::default(),
            tx_output_default: false,
            current_band_index: default_band_index,
            radio_frequency: None,
            band_cache: HashMap::new(),
            active_view: Self::load_persisted_view(),
            zoomed: false,
            placement: None,
            placement_cursor: 0,
            parked_since: None,
            park_coverage_last: None,
        };

        // Initialize audio monitoring if device specified
        if let Some(device) = app.audio_device.clone() {
            app.start_audio_monitoring(&device).await?;
        }

        info!("Terminal color capability: {:?}", app.color_capability);
        info!(
            "App initialized with station {}",
            app.station_info.call_sign
        );
        Ok(app)
    }

    /// Path to the persisted TUI state file (`~/.pancetta/tui_state.json`).
    /// `None` if the home directory can't be resolved.
    ///
    /// Test-only seam: honors a thread-local override (see
    /// [`Self::set_test_tui_state_path`]) so the unit test suite never reads
    /// or writes the operator's real state file — compiled out of
    /// production builds entirely (`#[cfg(test)]`), so this is a no-op for
    /// the real TUI.
    fn tui_state_path() -> Option<std::path::PathBuf> {
        #[cfg(test)]
        {
            if let Some(p) = TEST_TUI_STATE_PATH_OVERRIDE.with(|cell| cell.borrow().clone()) {
                return Some(p);
            }
        }
        dirs::home_dir().map(|h| h.join(".pancetta").join("tui_state.json"))
    }

    /// Test-only: point [`Self::tui_state_path`] at `path` for the calling
    /// thread only, so tests never touch `~/.pancetta/tui_state.json`.
    /// `#[tokio::test]` defaults to a current-thread runtime, so a value set
    /// here is visible for the rest of the test body (including any tasks
    /// it spawns) — unlike a process-global env var, a thread-local can't
    /// leak into (or race with) a *different* test running concurrently on
    /// another thread. Call this before constructing `App` or calling
    /// `cycle_view`, at the top of the test.
    #[cfg(test)]
    pub(crate) fn set_test_tui_state_path(path: std::path::PathBuf) {
        TEST_TUI_STATE_PATH_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
    }

    /// Load the operator's last-selected activity view from disk. Best-effort:
    /// a missing file, unreadable file, malformed JSON, or garbage value all
    /// fall back to `ActiveView::Operate` — never fails App construction.
    pub fn load_persisted_view() -> crate::view::ActiveView {
        Self::tui_state_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("active_view")
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })
            .map(|s| crate::view::ActiveView::from_str_or_default(&s))
            .unwrap_or(crate::view::ActiveView::Operate)
    }

    /// Cycle the active view forward (`v`) or backward (`V`), persisting the
    /// new value best-effort — a write failure (e.g. no home dir, read-only
    /// filesystem) is silently ignored and never panics or fails the TUI.
    pub fn cycle_view(&mut self, forward: bool) {
        self.active_view = if forward {
            self.active_view.next()
        } else {
            self.active_view.prev()
        };
        // Hunt view narrows `displayed_messages()` to CQs + directed-at-us
        // (see `displayed_messages`); a row the cursor/pin was sitting on
        // (e.g. a third-party exchange) can vanish from the list the instant
        // we switch views. Without this, `band_activity_scroll` would keep
        // pointing past the shrunk list until the next decode arrived and
        // happened to trigger a clamp — Space would silently select nothing
        // in the meantime. Re-clamp immediately on every view switch (also a
        // no-op, harmless call for the common case where the list didn't
        // shrink).
        self.clamp_band_activity_selection();
        if let Some(p) = Self::tui_state_path() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(
                &p,
                format!("{{\"active_view\":\"{}\"}}", self.active_view.as_str()),
            );
        }
        self.status_message = format!("View: {:?}", self.active_view);
    }

    /// Returns the `TuiCommand` to send as a result of this event, if any.
    /// `App` doesn't own the outgoing channel (only `TuiRunner` does, same
    /// as every key-event command send), so a click that needs to send one
    /// (click-to-park on the TX-placement strip) hands it back to the
    /// caller rather than sending it itself — mirroring how key events are
    /// dispatched in `tui_runner.rs`.
    pub async fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
    ) -> Result<Option<crate::tui_runner::TuiCommand>> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.scroll_down();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                return Ok(self.handle_left_click(mouse.column, mouse.row));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Task 19: click-to-focus and click-to-park. Maps a left-click at
    /// absolute terminal coordinates `(x, y)` to a panel + row via the
    /// pure `ui::hit_test` — the SAME rect map `ui::draw` renders from, so
    /// a click always lands on the panel boundary the operator actually
    /// sees. A hit on any panel other than TxPlacement sets `active_panel`
    /// and moves+pins that panel's cursor to the clicked row, mirroring
    /// Tab + arrow-key navigation (reusing each panel's existing pin
    /// method — no new pinning logic). A hit on the TxPlacement panel's
    /// openness-STRIP row (row 0; other instrument rows just focus the
    /// panel) parks at the clicked frequency via `click_park_tx_placement`.
    fn handle_left_click(&mut self, x: u16, y: u16) -> Option<crate::tui_runner::TuiCommand> {
        let content_area =
            crate::ui::content_area(Rect::new(0, 0, self.terminal_size.0, self.terminal_size.1));
        let (panel, row) = crate::ui::hit_test(
            self.active_view,
            self.zoomed,
            self.active_panel,
            content_area,
            x,
            y,
        )?;

        if panel != ActivePanel::TxPlacement {
            self.active_panel = panel;
            let row = row as usize;
            match panel {
                ActivePanel::BandActivity => {
                    let len = self.displayed_messages().len();
                    if len > 0 {
                        self.band_activity_scroll = row.min(len - 1);
                        self.pin_band_activity_selection();
                    }
                }
                ActivePanel::DxHunter => {
                    let len = self.displayed_dx_stations().len();
                    if len > 0 {
                        self.dx_hunter_scroll = row.min(len - 1);
                        self.repin_dx_hunter();
                    }
                }
                ActivePanel::Callers => {
                    let len = self.displayed_callers().len();
                    if len > 0 {
                        self.callers_scroll = row.min(len - 1);
                        self.repin_callers();
                    }
                }
                ActivePanel::QsoStatus => {
                    // `panel_row_offset`'s border+header subtraction assumes
                    // the multi-QSO table layout; with exactly one active
                    // QSO the panel instead renders a non-table detail view
                    // (issue #97 item 4). Harmless in practice: `row.min(0)`
                    // collapses to 0 regardless of what `row` resolved to,
                    // which is always the right (only) cursor position here.
                    if !self.active_qsos.is_empty() {
                        self.qso_cursor = row.min(self.active_qsos.len() - 1);
                        self.repin_qso_selection();
                    }
                }
                ActivePanel::TxPlacement => unreachable!("excluded by the guard above"),
            }
            return None;
        }

        // TxPlacement: focus the panel regardless of which row was clicked.
        self.active_panel = ActivePanel::TxPlacement;
        if row != 0 {
            // Only the openness strip (row 0) parks; the other instrument
            // rows (stream markers / BEST row / park line / freq axis)
            // just focus the panel, per the brief.
            return None;
        }
        self.click_park_tx_placement(x, content_area)
    }

    /// Row-0 (openness strip) click on the TX-placement instrument: map
    /// the clicked column to a frequency bin using the SAME column<->
    /// frequency scaling the strip renderer uses (`ui::tx_placement::
    /// freq_for_col` + `strip_width_for`, reused rather than
    /// re-derived), then park there — collision-aware per spec §2:
    /// a bin busy in BOTH windows (openness code 0) refuses to park;
    /// codes 1/2 (busy in exactly one window) park but name which window
    /// is busy; code 3 (fully clear) parks normally. Uses the EXISTING
    /// `TuiCommand::SetTxOffset` path (same one the `o` modal and the
    /// TxPlacement Enter-park key use) — no new command.
    fn click_park_tx_placement(
        &mut self,
        x: u16,
        content_area: Rect,
    ) -> Option<crate::tui_runner::TuiCommand> {
        let placement = self.placement.clone()?;
        let rect = crate::ui::view_rects(
            self.active_view,
            self.zoomed,
            self.active_panel,
            content_area,
        )
        .into_iter()
        .find(|(p, _)| *p == ActivePanel::TxPlacement)
        .map(|(_, r)| r)?;

        // Inner (border-stripped) geometry, mirroring `block.inner(area)`.
        if rect.width < 3 || x < rect.x + 1 {
            return None;
        }
        let inner_width = (rect.width - 2) as usize;
        let strip_width = crate::ui::tx_placement::strip_width_for(inner_width);
        let col = (x - (rect.x + 1)) as usize;
        if col >= strip_width {
            // Clicked the legend ("█both ▀E ▄O"), not the strip itself.
            return None;
        }

        let freq = crate::ui::tx_placement::freq_for_col(col, strip_width, placement.range)?;
        let n_bins = placement.openness.len();
        let bin = crate::ui::tx_placement::bin_index_for_freq(
            freq,
            placement.range,
            placement.bin_hz,
            n_bins,
        )?;
        let hz = freq.round().max(0.0) as u64;
        let code = placement.openness.get(bin).copied().unwrap_or(0);

        if code == 0 {
            self.status_message = format!("\u{26a0} {hz} Hz busy in both windows — not parking");
            return None;
        }

        self.tx_offset_hold_hz = Some(hz);
        self.tx_freq_mode = pancetta_core::TxFreqMode::Hold;
        self.parked_since = Some(chrono::Utc::now());
        self.status_message = match code {
            1 => format!("Parked at {hz} Hz (O) \u{2014} Even window busy"),
            2 => format!("Parked at {hz} Hz (E) \u{2014} Odd window busy"),
            _ => format!("Parked at {hz} Hz (E+O)"),
        };
        Some(crate::tui_runner::TuiCommand::SetTxOffset {
            offset_hz: Some(hz),
        })
    }

    pub async fn handle_resize(&mut self, width: u16, height: u16) -> Result<()> {
        self.terminal_size = (width, height);
        debug!("Terminal resized to {}x{}", width, height);
        Ok(())
    }

    pub async fn handle_audio_data(&mut self, data: Vec<f32>) -> Result<()> {
        self.process_audio_data(data).await
    }

    pub async fn handle_decoded_message(&mut self, message: DecodedMessageView) -> Result<()> {
        self.add_decoded_message(message).await
    }

    async fn start_audio_monitoring(&mut self, _device: &str) -> Result<()> {
        // Audio pipeline setup is handled by the coordinator, which creates
        // audio → DSP → FT8 → TUI channels before launching the TUI.
        // This method just sets the monitoring flag so the UI knows to expect data.
        // Standalone TUI operation (without coordinator) is not yet supported.
        self.is_monitoring = true;
        self.status_message = "Audio monitoring active (via coordinator)".to_string();
        info!("Audio monitoring flag set — pipeline managed by coordinator");
        Ok(())
    }

    pub async fn toggle_monitoring(&mut self) -> Result<()> {
        self.is_monitoring = !self.is_monitoring;
        self.status_message = if self.is_monitoring {
            "Audio monitoring enabled"
        } else {
            "Audio monitoring disabled"
        }
        .to_string();

        info!(
            "Audio monitoring: {}",
            if self.is_monitoring {
                "enabled"
            } else {
                "disabled"
            }
        );
        Ok(())
    }

    async fn process_audio_data(&mut self, data: Vec<f32>) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // Calculate audio level (RMS)
        let sum_squares: f32 = data.iter().map(|&x| x * x).sum();
        self.audio_level = (sum_squares / data.len() as f32).sqrt();

        // Waterfall data comes from the DSP pipeline via TuiMessage::WaterfallData
        // We only compute audio level here, not fake FFT data
        Ok(())
    }

    pub async fn add_decoded_message(&mut self, message: DecodedMessageView) -> Result<()> {
        debug!("Adding decoded message: {}", message.message);

        // Add to band activity
        self.decoded_messages.push_back(message.clone());

        // Preserve operator's highlight position. The display reverses
        // (newest first), and `band_activity_scroll` indexes into that
        // reversed view. A push_back makes a new entry the new "row 0",
        // so every previously-highlighted row's index shifts by one.
        // Without this bump, the operator highlights a CQ, a new decode
        // arrives ~15 seconds later, the highlight visually stays put
        // but now points at the freshly-arrived decode — Space-press
        // calls the wrong station. Bumping keeps them on the same
        // logical message. Special case: scroll == 0 means "track the
        // newest", which is what the operator wants — leave it alone.
        if self.band_activity_scroll > 0 {
            self.band_activity_scroll += 1;
        }

        // Limit message history. pop_front removes the oldest, which
        // doesn't shift indices counted from the back, so the scroll
        // position stays valid through pruning.
        while self.decoded_messages.len() > 1000 {
            self.decoded_messages.pop_front();
        }

        // Update DX stations list — but never list OURSELVES. Self-decodes of
        // our own transmissions (and frames whose extracted callsign resolves to
        // our station call) must not appear as a DX entry. The `.filter()` makes
        // the `if let` simply not match for our own call, skipping the update.
        // (#42; unified via is_our_call which also catches compound forms like
        // K5ARH/P or EA8/K5ARH)
        if let Some(call_sign) = message.call_sign.as_ref().filter(|c| !self.is_our_call(c)) {
            // Preserve existing grid if new message doesn't have one
            // (e.g., RR73/73 messages don't carry grid info)
            let grid_square = message.grid_square.clone().or_else(|| {
                self.dx_stations
                    .get(call_sign)
                    .and_then(|s| s.grid_square.clone())
            });
            // Audio offset where we heard this station, clamped into the
            // FT8 passband. Used by the Space call-target.
            let audio_offset_hz = (message.delta_freq as u64).clamp(200, 2500);
            let local_score = self.calculate_dx_priority(&message);

            // Merge into any existing entry rather than wholesale-replacing
            // it. A blanket `insert` would clobber network metadata
            // (rarity_tier / reporter_count / is_notable / confidence /
            // best_snr_network) every time the same callsign is re-decoded
            // locally, making rows churn between rich (network-scored) and
            // bare (local-only) on each 15s slot. We update the fields a
            // local decode actually knows about and leave network metadata
            // intact.
            match self.dx_stations.get_mut(call_sign) {
                Some(entry) => {
                    entry.grid_square = grid_square;
                    entry.frequency = message.frequency;
                    entry.mode = message.mode.clone();
                    entry.last_seen = message.timestamp;
                    entry.snr = message.snr;
                    entry.distance = message.distance;
                    entry.bearing = message.bearing;
                    entry.worked_before = message.worked_before;
                    entry.needed = message.needed;
                    entry.atno = message.atno;
                    entry.audio_offset_hz = Some(audio_offset_hz);
                    entry.slot_parity = message.slot_parity;
                    // A station previously known only from the network is
                    // now also heard locally → upgrade to Both. A
                    // local-only station stays Local.
                    entry.source = match entry.source {
                        SpotSource::Network | SpotSource::Both => SpotSource::Both,
                        SpotSource::Local => SpotSource::Local,
                    };
                    // Re-score from the richer of local vs whatever network
                    // metadata is still attached. For purely local entries
                    // this is just the local score; network spots keep
                    // their score raised on merge() and we don't lower it.
                    entry.priority_score = entry.priority_score.max(local_score);
                }
                None => {
                    self.dx_stations.insert(
                        call_sign.clone(),
                        DxStation {
                            call_sign: call_sign.clone(),
                            grid_square,
                            frequency: message.frequency,
                            mode: message.mode.clone(),
                            last_seen: message.timestamp,
                            snr: message.snr,
                            distance: message.distance,
                            bearing: message.bearing,
                            // Carried from the relay's CachedStationLookup
                            // check — same source as the autonomous scorer's
                            // duplicate penalty (band-scoped, uppercase-exact
                            // match).
                            worked_before: message.worked_before,
                            needed: message.needed,
                            atno: message.atno,
                            priority_score: local_score,
                            source: SpotSource::Local,
                            entity_name: None,
                            rarity_tier: None,
                            reporter_count: None,
                            is_notable: false,
                            notable_type: None,
                            confidence: None,
                            best_snr_network: None,
                            last_seen_network: None,
                            audio_offset_hz: Some(audio_offset_hz),
                            slot_parity: message.slot_parity,
                        },
                    );
                }
            }
        }

        // A new decode can grow/reorder both the DX-Hunter and Callers lists
        // (a new caller arrives at row 0; a higher-priority spot re-sorts the
        // DX list). Re-derive both cursors from their pinned callsigns so the
        // operator's Space/Enter target doesn't silently shift onto whoever
        // just appeared.
        self.clamp_band_activity_selection();
        self.clamp_dx_hunter_selection();
        self.clamp_callers_selection();

        self.status_message = format!("Decoded: {}", message.message);
        Ok(())
    }

    fn calculate_dx_priority(&self, message: &DecodedMessageView) -> u32 {
        // If the coordinator pre-computed a nuanced score from the real
        // `PriorityScorer` (continuous f64 mapped to [0, 1000]), use it
        // directly — it incorporates rarity, SNR, staleness, ATNO, and
        // needed-DXCC/grid with configured weights, matching the autonomous
        // scorer exactly. Fall back to the coarse bucket function only for
        // legacy/test paths where no pre-computed score is available.
        if let Some(pre_computed) = message.priority_score {
            return pre_computed;
        }
        // Fallback: coarse bucket function (ATNO > needed-DXCC > rarity > distance > SNR).
        crate::ui::dx_hunter::dx_priority_score(
            message.atno,
            message.needed,
            None,
            message.call_sign.as_deref().unwrap_or(""),
            message.distance,
            message.snr,
        )
    }

    fn scroll_up(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                if self.band_activity_scroll > 0 {
                    self.band_activity_scroll -= 1;
                }
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                if self.dx_hunter_scroll > 0 {
                    self.dx_hunter_scroll -= 1;
                }
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                if self.callers_scroll > 0 {
                    self.callers_scroll -= 1;
                }
                self.repin_callers();
            }
            _ => {}
        }
    }

    fn scroll_down(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                let max_scroll = self.displayed_messages().len().saturating_sub(1);
                if self.band_activity_scroll < max_scroll {
                    self.band_activity_scroll += 1;
                }
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                let max_scroll = self.displayed_dx_stations().len().saturating_sub(1);
                if self.dx_hunter_scroll < max_scroll {
                    self.dx_hunter_scroll += 1;
                }
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                let max_scroll = self.displayed_callers().len().saturating_sub(1);
                if self.callers_scroll < max_scroll {
                    self.callers_scroll += 1;
                }
                self.repin_callers();
            }
            _ => {}
        }
    }

    /// Pin the DX-Hunter selection to whatever callsign currently sits under
    /// `dx_hunter_scroll`. Called after a deliberate cursor move so a later
    /// list mutation re-derives the index from this callsign rather than
    /// snapping back to a stale position.
    fn repin_dx_hunter(&mut self) {
        self.dx_hunter_pinned_call = self
            .displayed_dx_stations()
            .get(self.dx_hunter_scroll)
            .map(|s| s.call_sign.clone());
    }

    /// Pin the Callers selection to whatever callsign currently sits under
    /// `callers_scroll`, and reset the reply override when the pinned caller
    /// changes. Called after a deliberate cursor move.
    fn repin_callers(&mut self) {
        let current = self
            .displayed_callers()
            .get(self.callers_scroll)
            .and_then(|m| m.call_sign.clone());
        self.callers_pinned_call = current.clone();
        if current != self.caller_override_for {
            self.caller_reply_override = None;
            self.caller_override_for = current;
        }
    }

    /// Number of rows a PageUp/PageDown moves the focused list.
    const SCROLL_PAGE: usize = 10;

    /// Jump the focused list to the top — for Band Activity that's the
    /// newest decode (realtime), since the list is newest-first. This is the
    /// "get me back to live" shortcut (Home / `g`), no more holding the arrow.
    pub fn scroll_to_top(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                self.band_activity_scroll = 0;
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                self.dx_hunter_scroll = 0;
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                self.callers_scroll = 0;
                self.repin_callers();
            }
            _ => {}
        }
    }

    /// Jump the focused list to the bottom — for Band Activity the oldest
    /// retained decode (End / `G`).
    pub fn scroll_to_bottom(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                self.band_activity_scroll = self.displayed_messages().len().saturating_sub(1);
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                self.dx_hunter_scroll = self.displayed_dx_stations().len().saturating_sub(1);
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                self.callers_scroll = self.displayed_callers().len().saturating_sub(1);
                self.repin_callers();
            }
            _ => {}
        }
    }

    /// Page toward the top (newest/realtime for Band Activity).
    pub fn page_up(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                self.band_activity_scroll =
                    self.band_activity_scroll.saturating_sub(Self::SCROLL_PAGE);
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                self.dx_hunter_scroll = self.dx_hunter_scroll.saturating_sub(Self::SCROLL_PAGE);
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                self.callers_scroll = self.callers_scroll.saturating_sub(Self::SCROLL_PAGE);
                self.repin_callers();
            }
            _ => {}
        }
    }

    /// Page toward the bottom (older entries).
    pub fn page_down(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                let max = self.displayed_messages().len().saturating_sub(1);
                self.band_activity_scroll =
                    (self.band_activity_scroll + Self::SCROLL_PAGE).min(max);
                self.pin_band_activity_selection();
            }
            ActivePanel::DxHunter => {
                let max = self.displayed_dx_stations().len().saturating_sub(1);
                self.dx_hunter_scroll = (self.dx_hunter_scroll + Self::SCROLL_PAGE).min(max);
                self.repin_dx_hunter();
            }
            ActivePanel::Callers => {
                let max = self.displayed_callers().len().saturating_sub(1);
                self.callers_scroll = (self.callers_scroll + Self::SCROLL_PAGE).min(max);
                self.repin_callers();
            }
            _ => {}
        }
    }

    pub fn clear_messages(&mut self) {
        self.decoded_messages.clear();
        self.band_activity_scroll = 0;
        self.clamp_band_activity_selection();
        self.status_message = "Messages cleared".to_string();
        info!("Cleared all decoded messages");
    }

    /// How long a first `x` press stays armed for a confirming second press
    /// (Task 20f).
    const CLEAR_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

    /// Confirm-on-`x`: the first press arms a 3-second confirmation window
    /// (and leaves a status hint) instead of clearing immediately; a second
    /// press WITHIN that window actually clears. A press after the window
    /// expired doesn't clear — it re-arms, restarting the 2-press sequence.
    /// Returns `true` iff decodes were actually cleared this call (the
    /// caller uses this to decide whether to also tell the coordinator to
    /// clear its side via `TuiCommand::ClearMessages`).
    pub fn try_clear_decodes(&mut self) -> bool {
        match self.clear_armed_at {
            Some(armed_at) if armed_at.elapsed() <= Self::CLEAR_CONFIRM_WINDOW => {
                self.clear_armed_at = None;
                self.clear_messages();
                true
            }
            _ => {
                self.clear_armed_at = Some(std::time::Instant::now());
                self.status_message = "press x again within 3s to clear decodes".to_string();
                false
            }
        }
    }

    fn cleanup_old_data(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::hours(24);

        // Remove old messages
        while let Some(front) = self.decoded_messages.front() {
            if front.timestamp < cutoff {
                self.decoded_messages.pop_front();
            } else {
                break;
            }
        }

        // Remove old DX stations
        self.dx_stations
            .retain(|_, station| station.last_seen > cutoff);
    }

    /// Called when the radio reports its actual frequency (via hamlib/rigctld).
    /// Updates radio_frequency for delta display. Does NOT change our target operating frequency.
    pub fn update_frequency(&mut self, freq: u64) {
        let freq_mhz = freq as f64 / 1_000_000.0;
        self.radio_frequency = Some(freq_mhz);
    }

    /// How long a cached per-band snapshot stays restorable. Return to a band
    /// within this window and its Callers / DX lists come back; older than this
    /// and the operator starts fresh (the stale snapshot is dropped).
    const BAND_CACHE_TTL_SECS: i64 = 600; // 10 minutes
    /// A DX Hunter spot drops off the displayed list this long after it was
    /// last heard (operator request). The underlying `dx_stations` store keeps
    /// it for the 24 h `cleanup_old_data` window (snapshots / worked tracking);
    /// this only governs what the operator sees as "current" DX.
    const DX_HUNTER_STALE_SECS: i64 = 180; // 3 minutes
    /// Cap on the number of bands kept in the snapshot cache so memory stays
    /// bounded regardless of how much the operator band-hops.
    const BAND_CACHE_MAX: usize = 4;

    /// Switch to the next band (higher frequency). Returns the new FT8 dial frequency in Hz.
    pub fn band_up(&mut self) -> u64 {
        let num_bands = self.config.bands.bands.len();
        if num_bands == 0 {
            return (self.station_info.operating_frequency * 1_000_000.0) as u64;
        }
        let old_band = self.config.bands.bands[self.current_band_index]
            .name
            .clone();
        self.current_band_index = (self.current_band_index + 1) % num_bands;
        self.apply_band_selection(Some(old_band))
    }

    /// Switch to the previous band (lower frequency). Returns the new FT8 dial frequency in Hz.
    pub fn band_down(&mut self) -> u64 {
        let num_bands = self.config.bands.bands.len();
        if num_bands == 0 {
            return (self.station_info.operating_frequency * 1_000_000.0) as u64;
        }
        let old_band = self.config.bands.bands[self.current_band_index]
            .name
            .clone();
        self.current_band_index = (self.current_band_index + num_bands - 1) % num_bands;
        self.apply_band_selection(Some(old_band))
    }

    /// Apply the current band selection, updating operating frequency.
    /// Returns the FT8 dial frequency in Hz.
    ///
    /// `leaving_band` is the name of the band we are switching AWAY from (the
    /// one whose live lists are about to be cleared), or `None` on the initial
    /// selection where there is nothing to stash. The leaving band's decoded
    /// messages and DX stations are snapshotted into `band_cache` (so a return
    /// within [`Self::BAND_CACHE_TTL_SECS`] can restore who was calling us),
    /// then ALL live lists (Band Activity, Callers — both derive from
    /// `decoded_messages` — and the DX Hunter) are cleared and their cursors /
    /// pins reset. If the band we are switching TO has a fresh cached snapshot,
    /// it is restored and the stale-cache entry is consumed.
    fn apply_band_selection(&mut self, leaving_band: Option<String>) -> u64 {
        // 1. Stash the band we're leaving so a return within the TTL restores it.
        if let Some(old) = leaving_band {
            if !self.decoded_messages.is_empty() || !self.dx_stations.is_empty() {
                self.band_cache.insert(
                    old,
                    BandSnapshot {
                        decoded_messages: self.decoded_messages.clone(),
                        dx_stations: self.dx_stations.clone(),
                        captured_at: Utc::now(),
                    },
                );
            }
        }

        // 2. Prune stale / overflow cache entries to keep memory bounded.
        self.prune_band_cache();

        let band = &self.config.bands.bands[self.current_band_index];
        let band_name = band.name.clone();
        // TODO(ft4): manual band-change dial is mode-unaware here — the TUI's
        // band table (`config.bands`) carries FT8 dial frequencies in MHz and
        // the active operating mode lives in the coordinator's [rig] config,
        // not threaded into the TUI. In FT4 mode the operator currently lands
        // on the FT8 dial via the manual `=`/`-` path. The autonomous band-hop
        // path IS mode-aware (coordinator/autonomous.rs ChangeBand handler,
        // via pancetta_core::Band::ft4_frequency / dial_for). Wiring the TUI
        // path needs the active mode + a per-band FT4 column threaded in.
        self.station_info.operating_frequency = band.ft8_frequency;
        let dial = (band.ft8_frequency * 1_000_000.0) as u64;
        let base_status = format!("Band: {} — {:.3} MHz", band_name, band.ft8_frequency);

        // 3. Clear every live list and reset cursors/pins.
        self.decoded_messages.clear();
        self.dx_stations.clear();
        self.band_activity_scroll = 0;
        self.dx_hunter_scroll = 0;
        self.dx_hunter_pinned_call = None;
        self.callers_scroll = 0;
        self.callers_pinned_call = None;
        self.caller_reply_override = None;
        self.caller_override_for = None;

        // 4. Restore a fresh snapshot for the band we're switching TO, if any.
        if let Some(snap) = self.band_cache.remove(&band_name) {
            let age_secs = (Utc::now() - snap.captured_at).num_seconds();
            if age_secs <= Self::BAND_CACHE_TTL_SECS {
                let caller_count = directed_caller_count(&snap.decoded_messages);
                self.decoded_messages = snap.decoded_messages;
                self.dx_stations = snap.dx_stations;
                let mins = (age_secs / 60).max(0);
                self.status_message = if caller_count > 0 {
                    format!(
                        "{} — restored {} caller{} from {}m ago",
                        base_status,
                        caller_count,
                        if caller_count == 1 { "" } else { "s" },
                        mins
                    )
                } else {
                    format!("{} — restored from {}m ago", base_status, mins)
                };
                return dial;
            }
            // else: too old — already removed; fall through to a fresh start.
        }

        self.status_message = base_status;
        dial
    }

    /// Drop band-cache entries older than the TTL, then evict the oldest until
    /// the cache is within [`Self::BAND_CACHE_MAX`].
    fn prune_band_cache(&mut self) {
        let now = Utc::now();
        self.band_cache
            .retain(|_, snap| (now - snap.captured_at).num_seconds() <= Self::BAND_CACHE_TTL_SECS);
        while self.band_cache.len() > Self::BAND_CACHE_MAX {
            if let Some(oldest) = self
                .band_cache
                .iter()
                .min_by_key(|(_, s)| s.captured_at)
                .map(|(k, _)| k.clone())
            {
                self.band_cache.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Returns the frequency delta between our expected frequency and the radio, if known.
    /// Positive means radio is higher than expected.
    pub fn frequency_delta_khz(&self) -> Option<f64> {
        self.radio_frequency
            .map(|radio_mhz| (radio_mhz - self.station_info.operating_frequency) * 1000.0)
    }

    /// Record a rig S-meter reading (hamlib STRENGTH convention: dB
    /// relative to S9). Batch 95: previously this clobbered
    /// `audio_level` (a 0.0-1.0 RMS ratio) with a dB value, which would
    /// have pegged/broken the audio gauge the moment a real reading
    /// arrived — nothing produced the message until now, so the bug was
    /// latent. S-meter and audio level are now separate fields.
    pub fn update_signal_strength(&mut self, db_over_s9: i32) {
        self.signal_strength_db = Some(db_over_s9);
        self.signal_strength_at = Some(Utc::now());
    }

    /// Record a live SWR reading (e.g. 1.3 = 1.3:1) from the rig while keyed.
    pub fn update_swr(&mut self, swr: f32) {
        self.swr = Some(swr);
        self.swr_at = Some(Utc::now());
    }

    /// SWR display ("SWR 1.3:1"), or `None` when there's no recent reading
    /// (rig stopped reporting > 5s ago — TX ended). Only sampled during TX, so
    /// this naturally clears shortly after unkey.
    pub fn swr_display(&self) -> Option<String> {
        const STALE_AFTER_SECS: i64 = 5;
        let swr = self.swr?;
        let at = self.swr_at?;
        if (Utc::now() - at).num_seconds() > STALE_AFTER_SECS {
            return None;
        }
        Some(format!("SWR {swr:.1}:1"))
    }

    /// S-meter display, or `None` when no reading exists or the last
    /// reading is stale (rig stopped reporting > 10s ago). Formatting
    /// follows the hamlib STRENGTH convention (0 dB = S9, 6 dB per
    /// S-unit below S9).
    pub fn s_meter_display(&self) -> Option<String> {
        let db = self.signal_strength_db?;
        let at = self.signal_strength_at?;
        if (Utc::now() - at).num_seconds() > SIGNAL_STRENGTH_STALE_AFTER_SECS {
            return None;
        }
        Some(format_s_meter(db))
    }

    /// Status-bar S-meter span text (Task 18 — demoted from the Station Info
    /// panel): `"S:{db:+}"` (raw signed dB relative to S9) when the last
    /// hamlib STRENGTH reading is fresh, else `"S:---"`. Reuses the exact
    /// same fields + staleness gate as `s_meter_display` (same data/logic);
    /// only the display FORMAT differs (raw signed dB, not the S-unit
    /// conversion) — that's what the status bar's compact single-span layout
    /// calls for.
    pub fn signal_strength_status_bar(&self) -> String {
        match (self.signal_strength_db, self.signal_strength_at) {
            (Some(db), Some(at))
                if (Utc::now() - at).num_seconds() <= SIGNAL_STRENGTH_STALE_AFTER_SECS =>
            {
                format!("S:{db:+}")
            }
            _ => "S:---".to_string(),
        }
    }

    /// Get the primary (first) QSO status, or a default standby entry.
    pub fn qso_status(&self) -> &QsoStatus {
        static DEFAULT: std::sync::LazyLock<QsoStatus> =
            std::sync::LazyLock::new(QsoStatus::default);
        self.qso_statuses.first().unwrap_or(&DEFAULT)
    }

    /// Get a mutable reference to the primary QSO, creating one if needed.
    pub fn qso_status_mut(&mut self) -> &mut QsoStatus {
        if self.qso_statuses.is_empty() {
            self.qso_statuses.push(QsoStatus::default());
        }
        &mut self.qso_statuses[0]
    }

    /// Apply an active-QSOs snapshot from the coordinator (Batch 94).
    ///
    /// Replaces BOTH views derived from the snapshot: the one-row banner
    /// (`active_qsos`) and the QSO-detail panel entries (`qso_statuses`).
    /// The sender owns the truth — completed/failed QSOs simply stop
    /// appearing in the next snapshot, so they leave the detail panel
    /// the same way they leave the banner. An empty snapshot clears the
    /// panel back to STANDBY.
    ///
    /// `pending` carries the cross-parity manual-call queue (#40) and is
    /// stored on `self.pending_calls` for the QSO Status panel renderer.
    pub fn apply_active_qsos(
        &mut self,
        qsos: Vec<ActiveQsoBanner>,
        pending: Vec<PendingCallBanner>,
    ) {
        self.qso_statuses = qsos
            .iter()
            .map(|q| QsoStatus {
                active: true,
                call_sign: Some(q.their_callsign.clone()),
                frequency: Some(q.frequency_hz),
                mode: Some("FT8".to_string()),
                state: Some(q.state.clone()),
                // TX gauge: their report of our signal.
                snr_tx: q.report_received,
                // RX gauge: measured SNR of their last message; fall back
                // to the report we sent (same quantity, coarser).
                snr_rx: q.snr_rx.or(q.report_sent),
                started_at: Some(q.started_at),
                last_tx: q.last_tx_at,
                last_rx: q.last_rx_at,
                last_tx_text: q.last_tx_text.clone(),
                last_rx_text: q.last_rx_text.clone(),
                report_sent: q.report_sent,
                report_received: q.report_received,
                exchange_count: q.exchange_count,
                qso_id: Some(q.qso_id.clone()),
                initiated_by: Some(q.initiated_by.clone()),
                ladder_labels: q.ladder_labels.clone(),
                ladder_ours: q.ladder_ours.clone(),
                ladder_index: q.ladder_index,
                now_line: q.now_line.clone(),
                next_line: q.next_line.clone(),
                call_count: q.call_count,
                max_calls: q.max_calls,
                watchdog_deadline: q.watchdog_deadline,
                dx_last_activity: q.dx_last_activity.clone(),
                hound: q.hound,
            })
            .collect();
        self.active_qsos = qsos;
        self.pending_calls = pending;
        // Batch 2 #5: re-derive the cursor from the pinned qso_id so the
        // selection tracks the SAME QSO across snapshots (the emit order is now
        // stable, but a QSO can still appear/disappear, shifting positions).
        self.clamp_qso_selection();
    }

    /// Re-derive `qso_cursor` from `qso_pinned_id` so the selection follows the
    /// same QSO across snapshots. Falls back to a positional clamp (and re-pins)
    /// when the pinned QSO is gone or nothing is pinned yet.
    fn clamp_qso_selection(&mut self) {
        if self.active_qsos.is_empty() {
            self.qso_cursor = 0;
            self.qso_pinned_id = None;
            return;
        }
        if let Some(ref pin) = self.qso_pinned_id {
            if let Some(idx) = self.active_qsos.iter().position(|q| &q.qso_id == pin) {
                self.qso_cursor = idx;
                return;
            }
        }
        // Pinned QSO gone (or none pinned): clamp the index and re-pin to it.
        self.qso_cursor = self.qso_cursor.min(self.active_qsos.len() - 1);
        self.qso_pinned_id = Some(self.active_qsos[self.qso_cursor].qso_id.clone());
    }

    /// Id of the currently selected QSO for management actions, or the
    /// sole QSO when exactly one is active. `None` when there are no
    /// active QSOs.
    pub fn selected_qso_id(&self) -> Option<String> {
        if self.active_qsos.is_empty() {
            return None;
        }
        let idx = self.qso_cursor.min(self.active_qsos.len() - 1);
        Some(self.active_qsos[idx].qso_id.clone())
    }

    /// The DX callsign of the currently selected QSO, for operator feedback
    /// on abort/re-send ("Aborting QSO with W1AW").
    pub fn selected_qso_callsign(&self) -> Option<String> {
        if self.active_qsos.is_empty() {
            return None;
        }
        let idx = self.qso_cursor.min(self.active_qsos.len() - 1);
        Some(self.active_qsos[idx].their_callsign.clone())
    }

    /// Move the QSO selection cursor up (toward index 0), saturating, and
    /// re-pin to the now-selected QSO so it sticks across snapshots.
    pub fn qso_cursor_up(&mut self) {
        self.qso_cursor = self.qso_cursor.saturating_sub(1);
        self.repin_qso_selection();
    }

    /// Move the QSO selection cursor down, clamped to the active set, and
    /// re-pin to the now-selected QSO.
    pub fn qso_cursor_down(&mut self) {
        if !self.active_qsos.is_empty() {
            self.qso_cursor = (self.qso_cursor + 1).min(self.active_qsos.len() - 1);
        }
        self.repin_qso_selection();
    }

    /// Pin the selection to the QSO currently under the cursor (so the next
    /// snapshot re-derives the index from this id, not the old position).
    fn repin_qso_selection(&mut self) {
        self.qso_pinned_id = self
            .active_qsos
            .get(self.qso_cursor)
            .map(|q| q.qso_id.clone());
    }

    /// Returns `true` when `call` resolves to our own station.
    ///
    /// Uses case-insensitive ASCII equality (matching the existing #42
    /// self-filter) because `pancetta_qso` is not a dependency of
    /// pancetta-tui. Guards against an empty/unconfigured callsign — if our
    /// call is blank we cannot tell what "our" station is, so we return
    /// `false` to avoid filtering every entry.
    fn is_our_call(&self, call: &str) -> bool {
        let ours = self.station_info.call_sign.trim();
        if ours.is_empty() {
            return false;
        }
        // Compound-callsign equivalence: K5ARH/P or EA8/K5ARH must also be
        // treated as "us". Replicate the base-call extraction that
        // pancetta_qso::exchange::callsigns_match uses without pulling in
        // that crate — split on '/', pick the longest token that contains
        // both a digit and an ASCII letter, compare case-insensitively.
        fn base_call(s: &str) -> &str {
            s.split('/')
                .max_by(|a, b| {
                    let shaped = |t: &str| {
                        t.len() >= 3
                            && t.bytes().any(|c| c.is_ascii_digit())
                            && t.bytes().any(|c| c.is_ascii_alphabetic())
                    };
                    match (shaped(a), shaped(b)) {
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        _ => a.len().cmp(&b.len()),
                    }
                })
                .unwrap_or(s)
        }
        call.eq_ignore_ascii_case(ours) || base_call(call).eq_ignore_ascii_case(base_call(ours))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_dx_spot(
        &mut self,
        callsign: String,
        freq: f64,
        mode: String,
        snr: i32,
        worked_before: bool,
        needed: bool,
        atno: bool,
    ) {
        // Never insert our own station into the DX Hunter. (#42 extension)
        if self.is_our_call(&callsign) {
            return;
        }
        let dx_station = DxStation {
            call_sign: callsign.clone(),
            grid_square: None,
            frequency: freq,
            mode,
            last_seen: Utc::now(),
            snr,
            distance: None,
            bearing: None,
            // Computed at the tui_relay layer against the coordinator's
            // CachedStationLookup (same source as the autonomous
            // scorer's duplicate penalty), keyed on the spot frequency.
            worked_before,
            needed,
            atno,
            priority_score: 0,
            source: SpotSource::Local,
            entity_name: None,
            rarity_tier: None,
            reporter_count: None,
            is_notable: false,
            notable_type: None,
            confidence: None,
            best_snr_network: None,
            last_seen_network: None,
            audio_offset_hz: None,
            slot_parity: None,
        };
        self.dx_stations.insert(callsign, dx_station);
        // Adding a spot can re-sort the DX-Hunter list; keep the cursor pinned.
        self.clamp_dx_hunter_selection();
    }

    pub fn add_error_message(&mut self, error: String) {
        self.status_message = format!("Error: {}", error);
    }

    /// Append a retained diagnostic event, evicting the oldest once the
    /// bounded history is full (docs/observability-diagnostics-plan.md
    /// Layer 1). Auto-follows: if the overlay is open and was already
    /// scrolled to the newest entry, the cursor advances to the new newest
    /// entry so a live event stream doesn't leave the operator staring at a
    /// stale scroll position.
    pub fn push_diagnostic_event(&mut self, event: DiagnosticEventRecord) {
        const MAX_DIAGNOSTIC_EVENTS: usize = 500;
        let was_at_end = self.diagnostic_events.len().saturating_sub(1) == self.diagnostics_scroll;
        self.diagnostic_events.push_back(event);
        while self.diagnostic_events.len() > MAX_DIAGNOSTIC_EVENTS {
            self.diagnostic_events.pop_front();
        }
        if was_at_end || self.show_diagnostics && self.diagnostic_events.len() == 1 {
            self.diagnostics_scroll = self.diagnostic_events.len().saturating_sub(1);
        }
    }

    /// Apply a fresh TX-placement snapshot (relayed from the coordinator's
    /// `MessageType::TxPlacementUpdate`). `v.received_at` is stamped by the
    /// relay layer (`tui_relay.rs`) at conversion time, since it owns the
    /// `pancetta_qso::PlacementSnapshot` -> `PlacementView` mapping; this
    /// method stores it as-is. Clamps `placement_cursor` into the new
    /// (possibly shorter) `slices` list — the same "cursor must survive a
    /// mutation" invariant Band Activity/DX Hunter/Callers established.
    ///
    /// **Parked-slice degradation warning (Task 14, edge-triggered):** when
    /// a park is active (`tx_offset_hold_hz.is_some()`), looks up the
    /// openness code at the parked frequency's bin in the NEW snapshot and
    /// compares it to `park_coverage_last` (the `(frequency, code)` as of
    /// the LAST call) — but ONLY when the tracked frequency still matches
    /// the currently-held one. A STRICT decrease at the SAME frequency
    /// (e.g. 3->1, 3->0, 2->0 — worse coverage than before) fires a
    /// one-shot `status_message` warning; an unchanged or improved code
    /// does not. `park_coverage_last` is updated to the new
    /// `(frequency, code)` every call regardless, so the fire is
    /// edge-triggered — a bin that stays degraded across many consecutive
    /// snapshots warns exactly once, on the transition, not on every update
    /// while still busy. No park active -> `park_coverage_last` resets to
    /// `None` (a fresh park always starts a clean baseline; see the field
    /// doc).
    ///
    /// **Re-park to a different frequency (Fix round 1):** the operator can
    /// re-park (Enter-park or the `o` modal) faster than a `PlacementView`
    /// snapshot arrives (~15s FT8 window), so `tx_offset_hold_hz` can change
    /// between two `apply_placement` calls without an intervening update at
    /// the OLD frequency. If the currently-held frequency doesn't match the
    /// frequency `park_coverage_last` was tracking, that's treated as a
    /// fresh baseline — same as `park_coverage_last == None` — so a
    /// comparison is never made across two different frequencies (which
    /// could otherwise fire a spurious warning, or mask a real one).
    pub fn apply_placement(&mut self, v: PlacementView) {
        let max_index = v.slices.len().saturating_sub(1);

        if let Some(hz) = self.tx_offset_hold_hz {
            let new_code = crate::ui::tx_placement::bin_index_for_freq(
                hz as f64,
                v.range,
                v.bin_hz,
                v.openness.len(),
            )
            .and_then(|b| v.openness.get(b).copied());

            // Only compare against the baseline if it was tracking THIS
            // frequency; a mismatch (re-parked since the last snapshot)
            // means there's no valid prior code to compare against here.
            let baseline_code = self
                .park_coverage_last
                .and_then(|(old_hz, old_code)| (old_hz == hz).then_some(old_code));

            if let (Some(old_code), Some(code)) = (baseline_code, new_code) {
                if code < old_code {
                    let busy = match code {
                        0 => "both",
                        1 => "E",
                        _ => "O",
                    };
                    let best = v
                        .slices
                        .first()
                        .map(|s| format!("{:.0}", s.offset_hz))
                        .unwrap_or_else(|| "\u{2014}".to_string());
                    self.status_message = format!(
                        "\u{26a0} parked {hz} now busy in {busy} \u{2014} \u{2460} {best} better"
                    );
                }
            }
            self.park_coverage_last = new_code.map(|code| (hz, code));
        } else {
            self.park_coverage_last = None;
        }

        self.placement = Some(v);
        self.placement_cursor = self.placement_cursor.min(max_index);
    }

    pub fn update_component_status(&mut self, component: String, status: String) {
        self.status_message = format!("{}: {}", component, status);
    }

    /// Append waterfall rows from a decoded window, keeping last 30 windows of data.
    pub fn push_waterfall_rows(&mut self, rows: Vec<Vec<f32>>) {
        // 15 rows per 15s FT8 cycle (1 row/sec) × 30 cycles = ~7.5 minutes of history
        const MAX_WATERFALL_ROWS: usize = 450;
        self.waterfall_data.extend(rows);
        if self.waterfall_data.len() > MAX_WATERFALL_ROWS {
            let excess = self.waterfall_data.len() - MAX_WATERFALL_ROWS;
            self.waterfall_data.drain(..excess);
        }
    }

    pub fn next_panel(&mut self) {
        self.active_panel = self.active_panel.next();
    }

    pub fn previous_panel(&mut self) {
        self.active_panel = self.active_panel.previous();
    }

    pub fn next_item(&mut self) {
        self.scroll_down();
    }

    pub fn previous_item(&mut self) {
        self.scroll_up();
    }

    pub fn next_page(&mut self) {
        for _ in 0..10 {
            self.scroll_down();
        }
    }

    pub fn previous_page(&mut self) {
        for _ in 0..10 {
            self.scroll_up();
        }
    }

    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
        if self.help_visible {
            self.status_message = "Help — press Escape or ? to close".to_string();
        } else {
            self.status_message = "Pancetta TUI Ready".to_string();
        }
    }

    /// Decoded messages in display order: directed-at-us first (pinned),
    /// then everything else in newest-first recency. Both the Band Activity
    /// renderer and `get_selected_station` walk this same ordering so
    /// scroll indices stay in sync between visible row and selected
    /// callsign — without that, an arriving directed-at-us decode would
    /// silently slide the highlight onto the wrong call.
    pub fn displayed_messages(&self) -> Vec<&DecodedMessageView> {
        let mut directed: Vec<&DecodedMessageView> = self
            .decoded_messages
            .iter()
            .rev()
            .filter(|m| m.is_directed_at_us)
            .collect();
        let mut others: Vec<&DecodedMessageView> = self
            .decoded_messages
            .iter()
            .rev()
            .filter(|m| !m.is_directed_at_us)
            .collect();
        // Hunt view narrows the non-directed rows to CQs only (DXpedition /
        // rare-station chasing wants CQ callers, not third-party exchange
        // noise). Directed-at-us rows are always kept regardless of view —
        // someone calling us must never be hidden. This lives here (not in
        // the renderer) so the cursor, the renderer, and Space all agree on
        // the same filtered list.
        if self.active_view == crate::view::ActiveView::Hunt {
            others.retain(|m| m.message.trim_start().starts_with("CQ"));
        }
        directed.extend(others);
        directed
    }

    /// DX stations in display order — the single source of truth for both
    /// the DX Hunter renderer and `get_selected_station`. Walk this exact
    /// ordering in both places so the highlighted row is always the Space
    /// call-target; if the renderer and the chooser sorted differently
    /// (the historic bug: renderer by `priority_score`, chooser by
    /// `last_seen`), Space would call a station the operator never
    /// highlighted.
    ///
    /// Comparator is a stable total order:
    ///   1. needed first (`worked_before == false` before `== true`)
    ///   2. `priority_score` descending
    ///   3. `snr` descending
    ///   4. `frequency` ascending
    ///   5. `call_sign` ascending (final tiebreak → deterministic)
    pub fn displayed_dx_stations(&self) -> Vec<&DxStation> {
        // Drop spots not heard within the staleness window — a station last
        // heard minutes ago is no longer "current" DX to chase.
        let cutoff = Utc::now() - chrono::Duration::seconds(Self::DX_HUNTER_STALE_SECS);
        // Belt-and-suspenders: even if our own callsign somehow slipped into
        // dx_stations (e.g. a stale entry from before #42 was extended), never
        // surface it to the operator at render time. (#42 extension)
        let our_call = self.station_info.call_sign.clone();
        let mut list: Vec<&DxStation> = self
            .dx_stations
            .values()
            .filter(|s| s.last_seen > cutoff)
            .filter(|s| {
                let ours = our_call.trim();
                ours.is_empty() || !s.call_sign.eq_ignore_ascii_case(ours)
            })
            .collect();
        list.sort_by(|a, b| {
            a.worked_before
                .cmp(&b.worked_before)
                .then(b.priority_score.cmp(&a.priority_score))
                .then(b.snr.cmp(&a.snr))
                .then(
                    a.frequency
                        .partial_cmp(&b.frequency)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(a.call_sign.cmp(&b.call_sign))
        });
        list
    }

    /// Get the callsign, audio offset (Hz, within FT8 passband), and slot
    /// parity of the currently selected station. Returns the AUDIO frequency
    /// offset, not the dial frequency — the TransmitRequest pipeline expects
    /// an absolute audio frequency in 200-2500 Hz, and the modulator validates
    /// against MAX_FREQUENCY_DEVIATION = 2500 Hz. The slot parity is `Some`
    /// when the station was decoded from a Band Activity message (so we know
    /// which 15-second slot they transmit on), and `None` for DX cluster spots.
    ///
    /// Works from both Band Activity (decoded messages) and DX Hunter (spots).
    pub fn get_selected_station(
        &self,
    ) -> Option<(String, u64, Option<pancetta_core::slot::SlotParity>)> {
        match self.active_panel {
            ActivePanel::BandActivity => {
                // Walk the same ordering the renderer does — directed-first,
                // then by recency.
                let displayed = self.displayed_messages();
                let msg = displayed.get(self.band_activity_scroll)?;
                let callsign = msg.call_sign.as_ref()?;
                if callsign.is_empty() {
                    return None;
                }
                // delta_freq is the audio offset in Hz where the signal was
                // decoded. Clamp into [200, 2500] since some decoders produce
                // out-of-range values for split-VFO or reference markers.
                let audio_hz = (msg.delta_freq as u64).clamp(200, 2500);
                Some((callsign.clone(), audio_hz, msg.slot_parity))
            }
            ActivePanel::DxHunter => {
                // Walk the SAME ordering the renderer draws (needed-first,
                // priority desc, then snr/freq/call tiebreaks) so the
                // highlighted row is exactly the Space call-target.
                let displayed = self.displayed_dx_stations();
                let station = displayed.get(self.dx_hunter_scroll)?;
                if station.call_sign.is_empty() {
                    return None;
                }
                // Local decodes carry the audio offset where we actually
                // heard the station — reply there. Network-only DX-cluster
                // spots have no passband info, so fall back to the FT8
                // calling convention (1500 Hz); tuning is then the
                // operator's job. Slot parity is `Some` only for local
                // decodes (network spots can't tell us the slot).
                let audio_hz = station.audio_offset_hz.unwrap_or(1500);
                Some((station.call_sign.clone(), audio_hz, station.slot_parity))
            }
            ActivePanel::Callers => {
                let displayed = self.displayed_callers();
                let msg = displayed.get(self.callers_scroll)?;
                let callsign = msg.call_sign.as_ref()?;
                if callsign.is_empty() {
                    return None;
                }
                // delta_freq is the audio offset where we heard them; reply
                // there. Clamp into the FT8 passband like the Band Activity arm.
                let audio_hz = (msg.delta_freq.round() as u64).clamp(200, 2500);
                Some((callsign.clone(), audio_hz, msg.slot_parity))
            }
            _ => None,
        }
    }

    pub fn activate_selected(&mut self) {
        if let Some((callsign, _freq, _parity)) = self.get_selected_station() {
            self.status_message = format!("Calling {}...", callsign);
        } else {
            self.status_message = "No station selected".to_string();
        }
    }

    /// The most-recent decode **directed at us** from `callsign`, if any.
    /// This is the same notion of "directed at us" the Callers panel uses
    /// (`is_directed_at_us`), filtered to a single callsign. `decoded_messages`
    /// is push-back (oldest→newest) so we walk it in reverse and take the first
    /// match. Used by [`Self::resolve_space_action`] to decide whether Space
    /// should answer a CQ (grid) or reply at the correct exchange step.
    pub fn last_directed_at_us_from(&self, callsign: &str) -> Option<&DecodedMessageView> {
        self.decoded_messages.iter().rev().find(|m| {
            m.is_directed_at_us
                && m.call_sign
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(callsign))
        })
    }

    /// Resolve what the Space key should do for the current global focus
    /// (Task 3 — `App::focused_callsign()`, NOT any one panel's own cursor),
    /// unifying the Space and Callers-Enter paths into "do the right next
    /// thing". Returns `None` when nothing is focused.
    ///
    /// - If the focused callsign has a most-recent decode directed at us (a
    ///   grid/report/R-report/RR73/73), classify it with the SAME
    ///   [`classify_caller_reply`] logic the Callers panel uses and return
    ///   [`SpaceAction::Reply`] at that smart-default step (their `RR73` → we
    ///   send `73`, their grid → we send our report, etc.).
    /// - Otherwise (a pure CQer, or a DX-cluster spot we've never heard call
    ///   us) return [`SpaceAction::Call`] — the historical Space behavior of
    ///   answering their CQ at the grid step.
    /// - **Fix round 2 (operator-confirmed 2026-07-03):** if neither of the
    ///   above applies (no directed-at-us reply pending) AND the focused
    ///   callsign already has a live QSO ([`Self::is_engaged`]), Space must
    ///   NOT re-initiate/supersede it — that would blow away an in-progress
    ///   (possibly autonomous) exchange. Returns `None` (the same "nothing to
    ///   do" signal used when no station is focused at all); the caller
    ///   surfaces an operator-facing status message naming the callsign. The
    ///   operator must explicitly cancel the QSO first.
    ///
    /// The reply target's frequency/parity come from the directed decode itself
    /// (where we actually heard them) rather than the selected-row frequency,
    /// so a reply always lands on the right passband even if the operator
    /// selected the station from the DX Hunter (network-spot) row.
    ///
    /// The Call-branch frequency/parity fall back to whatever the active
    /// panel's own cursor resolution knows about this same station (the
    /// common case — a panel's pin and cursor stay in sync with its own
    /// focus) and otherwise to the FT8 calling-frequency default (1500 Hz,
    /// unknown parity) — e.g. focus resolved from the QSO Status panel,
    /// which has no cursor-based frequency lookup of its own.
    pub fn resolve_space_action(&self) -> Option<SpaceAction> {
        let callsign = self.focused_callsign()?;

        if let Some(msg) = self.last_directed_at_us_from(&callsign) {
            let step = classify_caller_reply(&msg.message, &self.station_info.call_sign);
            // Reply on the passband where we actually heard them, on their slot
            // parity if we know it — mirrors the Callers-Enter reply target.
            let reply_freq = (msg.delta_freq.round() as u64).clamp(200, 2500);
            return Some(SpaceAction::Reply {
                callsign,
                frequency: reply_freq,
                dx_parity: msg.slot_parity,
                step,
                snr: Some(msg.snr as f32),
            });
        }

        // Fix round 2 (operator-confirmed 2026-07-03): Space must never
        // re-initiate/supersede a station we already have a live QSO with.
        // With no directed-at-us reply pending (the branch above), there is
        // nothing safe left for Space to do — force the operator to cancel
        // the QSO explicitly instead of silently superseding it. `None` is
        // the same "nothing to do" signal already used elsewhere in this
        // function (e.g. no station focused at all); the caller resolves the
        // operator-facing message.
        if self.is_engaged(&callsign) {
            return None;
        }

        let (frequency, dx_parity) = self
            .get_selected_station()
            .filter(|(c, _, _)| pancetta_core::callsign::callsigns_match(c, &callsign))
            .map(|(_, f, p)| (f, p))
            .or_else(|| {
                // `get_selected_station()` has no arm for `ActivePanel::QsoStatus`
                // (and its filter above only matches when it DOES resolve the
                // focused callsign), but `focused_callsign()` can still resolve
                // to an already-engaged station via `self.active_qsos`. Falling
                // all the way through to the (1500, None) default in that case
                // would discard the QSO's real TX frequency/parity and — via
                // `respond_to_cq_with`'s unfiltered `supersede_active_qsos_for`
                // — could blow away a live QSO to start a fresh Manual one at a
                // bogus 1500 Hz. Mirror exactly how `focused_callsign` resolves
                // the QsoStatus panel: look the callsign up in `active_qsos` and
                // use ITS real frequency/parity.
                self.active_qsos
                    .iter()
                    .find(|q| {
                        pancetta_core::callsign::callsigns_match(&q.their_callsign, &callsign)
                    })
                    .map(|q| (q.frequency_hz.round() as u64, q.tx_parity))
            })
            .unwrap_or((1500, None));

        Some(SpaceAction::Call {
            callsign,
            frequency,
            dx_parity,
        })
    }

    // === Callers panel ====================================================

    /// Stations currently calling US, in display order (newest first), one row
    /// per callsign (newest decode wins). Source is `decoded_messages`
    /// filtered to `is_directed_at_us`. Both the Callers renderer and
    /// `get_selected_station(Callers)` walk this exact list so the highlighted
    /// row is always the Enter reply-target.
    pub fn displayed_callers(&self) -> Vec<&DecodedMessageView> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<&DecodedMessageView> = Vec::new();
        // newest-first: iterate the deque in reverse (it is push-back oldest→newest)
        for msg in self.decoded_messages.iter().rev() {
            if !msg.is_directed_at_us {
                continue;
            }
            let Some(call) = msg.call_sign.as_ref() else {
                continue;
            };
            if call.is_empty() {
                continue;
            }
            let key = call.to_uppercase();
            if seen.insert(key) {
                out.push(msg);
            }
        }
        out
    }

    /// Re-derive `callers_scroll` from the pinned callsign and reset the reply
    /// override if the selected caller changed (so each newly-selected caller
    /// starts at its own smart default). Call this on EVERY path that can move
    /// the cursor or mutate the Callers list — Up/Down, Home/End/PgUp/PgDn,
    /// and decode arrival (`add_decoded_message`).
    ///
    /// The Callers list is newest-first and grows at row 0 as new callers
    /// arrive, so a bare positional index would slide the operator's selection
    /// (and the Enter reply-target) onto whoever just called. To prevent that
    /// we pin the SELECTED caller's callsign: if the pinned call is still in
    /// the list we re-point `callers_scroll` at its new index; if it's gone we
    /// clamp to the current bounds and adopt whatever caller now sits there.
    pub fn clamp_callers_selection(&mut self) {
        // Snapshot the callsigns in display order (owned) so we can mutate the
        // cursor without holding a borrow of `self` through `displayed_callers`.
        let calls: Vec<Option<String>> = self
            .displayed_callers()
            .iter()
            .map(|m| m.call_sign.clone())
            .collect();
        let len = calls.len();

        // First, re-derive the index from the pinned callsign so a list
        // growth/re-order doesn't retarget the operator.
        if let Some(ref pin) = self.callers_pinned_call {
            if let Some(pos) = calls
                .iter()
                .position(|c| c.as_deref() == Some(pin.as_str()))
            {
                self.callers_scroll = pos;
            }
        }

        let max = len.saturating_sub(1);
        if self.callers_scroll > max {
            self.callers_scroll = max;
        }
        // Adopt whatever caller the cursor now resolves to as the new pin.
        let current = calls.get(self.callers_scroll).cloned().flatten();
        self.callers_pinned_call = current.clone();
        if current != self.caller_override_for {
            self.caller_reply_override = None;
            self.caller_override_for = current;
        }
    }

    /// Re-derive `dx_hunter_scroll` from the pinned callsign so a list
    /// re-sort/growth doesn't silently retarget the Space call-target. Mirror
    /// of [`clamp_callers_selection`] for the DX-Hunter panel. Call this on
    /// every cursor-move and on every DX-list mutation (decode arrival,
    /// network-spot merge).
    ///
    /// If the pinned callsign is still present we re-point the cursor at its
    /// new index; if it left the list we clamp to bounds and adopt whatever
    /// station now sits under the cursor as the new pin.
    pub fn clamp_dx_hunter_selection(&mut self) {
        // Snapshot callsigns in display order (owned) to avoid holding a borrow
        // of `self` while we mutate the cursor.
        let calls: Vec<String> = self
            .displayed_dx_stations()
            .iter()
            .map(|s| s.call_sign.clone())
            .collect();
        let len = calls.len();

        if let Some(ref pin) = self.dx_hunter_pinned_call {
            if let Some(pos) = calls.iter().position(|c| c == pin) {
                self.dx_hunter_scroll = pos;
            }
        }

        let max = len.saturating_sub(1);
        if self.dx_hunter_scroll > max {
            self.dx_hunter_scroll = max;
        }
        self.dx_hunter_pinned_call = calls.get(self.dx_hunter_scroll).cloned();
    }

    /// The station under the operator's attention: the ACTIVE panel's pinned
    /// (or cursor-resolved) callsign. One focus, shared by every panel —
    /// renderers highlight it everywhere via `is_focused`.
    pub fn focused_callsign(&self) -> Option<String> {
        match self.active_panel {
            ActivePanel::BandActivity => self
                .band_activity_pinned_call
                .clone()
                .or_else(|| self.get_selected_station().map(|(call, _, _)| call)),
            ActivePanel::DxHunter => self.dx_hunter_pinned_call.clone().or_else(|| {
                self.displayed_dx_stations()
                    .get(self.dx_hunter_scroll)
                    .map(|s| s.call_sign.clone())
            }),
            ActivePanel::Callers => self.callers_pinned_call.clone().or_else(|| {
                self.displayed_callers()
                    .get(self.callers_scroll)
                    .and_then(|m| m.call_sign.clone())
            }),
            ActivePanel::QsoStatus => self
                .active_qsos
                .get(self.qso_cursor)
                .map(|q| q.their_callsign.clone()),
            // The instrument has no "focused station" concept of its own —
            // its own selection cursor is `placement_cursor` (a slice index,
            // not a callsign).
            ActivePanel::TxPlacement => None,
        }
    }

    /// Compound-aware "is this callsign the current focus?"
    pub fn is_focused(&self, call: &str) -> bool {
        self.focused_callsign()
            .is_some_and(|f| pancetta_core::callsign::callsigns_match(&f, call))
    }

    /// Is this callsign one of our current active-QSO partners (an "engaged"
    /// stream)? Engaged stations get the tier-2 highlight (green ● + underline)
    /// in every list — visible even while the focus is elsewhere (multi-TX).
    pub fn is_engaged(&self, call: &str) -> bool {
        self.active_qsos
            .iter()
            .any(|q| pancetta_core::callsign::callsigns_match(&q.their_callsign, call))
    }

    /// Pin the Band Activity selection to the callsign currently under the
    /// cursor. Call after every deliberate cursor move.
    pub fn pin_band_activity_selection(&mut self) {
        self.band_activity_pinned_call = self
            .displayed_messages()
            .get(self.band_activity_scroll)
            .and_then(|m| m.call_sign.clone());
    }

    /// Re-derive `band_activity_scroll` from the pinned callsign after any
    /// list mutation. Falls back to a clamp (and clears the pin) if the
    /// pinned callsign left the list.
    pub fn clamp_band_activity_selection(&mut self) {
        // Snapshot callsigns in display order (owned) to avoid holding a borrow
        // of `self` while we mutate the cursor.
        let calls: Vec<Option<String>> = self
            .displayed_messages()
            .iter()
            .map(|m| m.call_sign.clone())
            .collect();
        let len = calls.len();

        let mut pinned_aged_out = false;
        if let Some(ref pin) = self.band_activity_pinned_call {
            if let Some(idx) = calls.iter().position(|call| {
                call.as_deref()
                    .is_some_and(|c| pancetta_core::callsign::callsigns_match(c, pin))
            }) {
                self.band_activity_scroll = idx;
                return;
            }
            // Pin was explicitly set but aged out of the list; clear it and re-adopt at scroll
            self.band_activity_pinned_call = None;
            pinned_aged_out = true;
        }
        let max = len.saturating_sub(1);
        self.band_activity_scroll = self.band_activity_scroll.min(max);
        // If the pinned call aged out, re-adopt whatever now sits at the resolved index
        if pinned_aged_out {
            self.band_activity_pinned_call =
                calls.get(self.band_activity_scroll).and_then(|m| m.clone());
        }
    }

    /// The currently-selected caller, if any.
    pub fn selected_caller(&self) -> Option<&DecodedMessageView> {
        self.displayed_callers().get(self.callers_scroll).copied()
    }

    /// The reply step to use for the selected caller: the operator override if
    /// set, otherwise the smart default classified from the caller's last
    /// directed message.
    pub fn current_caller_reply_step(&self) -> pancetta_core::ResponseStep {
        let smart = self
            .selected_caller()
            .map(|m| classify_caller_reply(&m.message, &self.station_info.call_sign))
            .unwrap_or_default();
        self.caller_reply_override.unwrap_or(smart)
    }

    /// Cycle the Callers reply override forward (Right) or backward (Left)
    /// through the ladder. Initializes from the current smart default on the
    /// first press, then steps from there.
    pub fn cycle_caller_reply(&mut self, forward: bool) {
        use pancetta_core::ResponseStep::*;
        const LADDER: [pancetta_core::ResponseStep; 5] =
            [Grid, Report, ReportAck, Rr73, SeventyThree];
        let current = self.current_caller_reply_step();
        let idx = LADDER.iter().position(|s| *s == current).unwrap_or(0);
        let next = if forward {
            (idx + 1) % LADDER.len()
        } else {
            (idx + LADDER.len() - 1) % LADDER.len()
        };
        self.caller_reply_override = Some(LADDER[next]);
        // Pin the override to the selected caller so a later selection change
        // resets it.
        self.caller_override_for = self.selected_caller().and_then(|m| m.call_sign.clone());
    }

    /// Signal report (dB) we would send the selected caller, derived from the
    /// decode's SNR with the same round/clamp/default the QSO engine uses.
    pub fn caller_report_value(&self) -> i8 {
        self.selected_caller()
            .map(|m| (m.snr as f32).round() as i8)
            .map(|r| r.clamp(-30, 50))
            .unwrap_or(-15)
    }

    /// `true` if `callsign` is in an active QSO of ours (MINE flag).
    pub fn is_caller_mine(&self, callsign: &str) -> bool {
        self.active_qsos
            .iter()
            .any(|q| q.their_callsign.eq_ignore_ascii_case(callsign))
    }

    /// `true` if `callsign` appears to be mid-exchange with a THIRD party
    /// (BUSY flag): seen within the last ~90s in a 3-token `<to> <from>
    /// <payload>` exchange where neither party is us and the payload is a
    /// committed report/RR73/RRR/73. Mirrors
    /// `pancetta_qso::autonomous::third_party_exchange_callsigns` /
    /// `is_exchange_payload` (re-implemented here because `pancetta-tui` does
    /// not depend on `pancetta-qso`); the canonical versions are `pub` + unit
    /// tested in that crate to guard both copies against drift.
    pub fn is_caller_busy(&self, callsign: &str) -> bool {
        const BUSY_WINDOW_SECS: i64 = 90;
        let now = chrono::Utc::now();
        let our = self.station_info.call_sign.to_uppercase();
        let target = callsign.to_uppercase();
        self.decoded_messages.iter().any(|m| {
            if now.signed_duration_since(m.timestamp).num_seconds() > BUSY_WINDOW_SECS {
                return false;
            }
            third_party_exchange_participants(&m.message, &our).contains(&target)
        })
    }

    pub fn get_input_text(&self) -> String {
        self.tx_input_buffer.clone()
    }

    pub fn clear_input(&mut self) {
        self.tx_input_buffer.clear();
        self.tx_input_cursor = 0;
    }

    pub fn input_char(&mut self, c: char) {
        let c = c.to_ascii_uppercase();
        if self.tx_input_buffer.len() < 13 {
            self.tx_input_buffer.insert(self.tx_input_cursor, c);
            self.tx_input_cursor += 1;
            self.status_message = format!("TX: {}", self.tx_input_buffer);
        }
    }

    pub fn delete_char(&mut self) {
        if self.tx_input_cursor > 0 {
            self.tx_input_cursor -= 1;
            self.tx_input_buffer.remove(self.tx_input_cursor);
            self.status_message = format!("TX: {}", self.tx_input_buffer);
        }
    }

    /// Enter free-text compose mode. Shows the TX-message input line; while
    /// active, Char/Backspace edit the buffer and command letters are inert.
    pub fn enter_compose_mode(&mut self) {
        self.compose_mode = true;
        self.status_message = "Compose TX message — type, Enter to send, Esc to cancel".to_string();
    }

    /// Leave compose mode, discarding the in-progress buffer.
    pub fn cancel_compose_mode(&mut self) {
        self.compose_mode = false;
        self.clear_input();
        self.status_message = "Compose cancelled".to_string();
    }

    /// A small compose prompt for the status / input line, e.g.
    /// `TX> CQ K5ARH EM00_`. Returns `None` when not composing.
    pub fn compose_prompt(&self) -> Option<String> {
        if self.compose_mode {
            Some(format!("TX> {}_", self.tx_input_buffer))
        } else {
            None
        }
    }

    pub fn update_autonomous_status(&mut self, status: AutonomousStatus) {
        self.autonomous_status = Some(status);
    }

    /// Install the audio device lists enumerated by the coordinator into
    /// the device-selection picker. `current_output` (the configured
    /// output device name) is pre-selected so the picker opens on the
    /// active choice. Matched case-insensitively against the enumerated
    /// names; "default"/empty falls back to the OS-default entry.
    pub fn set_audio_devices(
        &mut self,
        input: Vec<(String, bool)>,
        output: Vec<(String, bool)>,
        current_output: Option<String>,
    ) {
        self.device_selection.input_devices = input;
        self.device_selection.output_devices = output;

        // Pre-select the current output device.
        let want = current_output.unwrap_or_default();
        let want_default = want.is_empty() || want.eq_ignore_ascii_case("default");
        let out_idx = self
            .device_selection
            .output_devices
            .iter()
            .position(|(name, is_default)| {
                if want_default {
                    *is_default
                } else {
                    name.eq_ignore_ascii_case(&want)
                }
            })
            .unwrap_or(0);
        self.device_selection.selected_output_idx = out_idx;

        // Default the input cursor to the OS-default input if present.
        let in_idx = self
            .device_selection
            .input_devices
            .iter()
            .position(|(_, is_default)| *is_default)
            .unwrap_or(0);
        self.device_selection.selected_input_idx = in_idx;
    }

    pub fn toggle_autonomous(&mut self) {
        if let Some(ref mut status) = self.autonomous_status {
            status.enabled = !status.enabled;
            self.status_message = if status.enabled {
                "Autonomous mode enabled".to_string()
            } else {
                "Autonomous mode disabled".to_string()
            };
            info!(
                "Autonomous mode: {}",
                if status.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        } else {
            // No live status yet (the autonomous loop pushes one per
            // 15s slot). The toggle command still goes to the
            // coordinator; the runtime gate flips regardless.
            self.status_message = "Autonomous toggle sent (waiting for live status)".to_string();
        }
    }

    /// Toggle local Fox-mode mirror flag for optimistic TUI feedback.
    ///
    /// The coordinator's `SetFoxMode` handler is the authority; this method
    /// only flips the local bool so the title-bar "FOX" chip appears
    /// immediately without waiting for a round-trip. The coordinator echoes
    /// the authoritative state via the standard `StatusUpdate` path.
    pub fn toggle_fox_mode(&mut self) {
        self.fox_mode = !self.fox_mode;
        self.status_message = if self.fox_mode {
            "Fox mode ON — calling CQ + answering callers (Shift+X to disengage)".to_string()
        } else {
            "Fox mode OFF".to_string()
        };
        info!(
            "Fox mode: {}",
            if self.fox_mode { "enabled" } else { "disabled" }
        );
    }

    /// Merge live spot groups from cqdx.io into the DX station list.
    ///
    /// Network spots used to land with `priority_score: 0`, so they always
    /// sorted to the bottom of the DX Hunter list regardless of how rare
    /// they were. We now run the richer `dx_hunter::calculate_dx_priority`
    /// scorer — which weights rarity_tier, distance, SNR and recency — so a
    /// "legendary"/"very_rare" cluster spot ranks where it belongs.
    pub fn merge_spot_groups(&mut self, spots: &[crate::tui_runner::CqdxSpotInfo]) {
        let our_grid = self.station_info.grid_square.clone();
        for spot in spots {
            // Never insert our own callsign into the DX Hunter via network
            // spots (e.g. a PSKReporter/cqdx monitor reporting our own TX).
            // (#42 extension)
            if self.is_our_call(&spot.dx_call) {
                continue;
            }
            let entry = self
                .dx_stations
                .entry(spot.dx_call.clone())
                .or_insert_with(|| DxStation {
                    call_sign: spot.dx_call.clone(),
                    grid_square: spot.grid.clone(),
                    frequency: spot.frequency_hz as f64 / 1_000_000.0,
                    mode: spot.mode.clone(),
                    last_seen: chrono::Utc::now(),
                    snr: spot.best_snr.unwrap_or(0),
                    distance: None,
                    bearing: None,
                    worked_before: false,
                    needed: spot.needed,
                    atno: spot.atno,
                    priority_score: 0,
                    source: SpotSource::Network,
                    entity_name: Some(spot.entity_name.clone()).filter(|s| !s.is_empty()),
                    rarity_tier: Some(spot.rarity_tier.clone()),
                    reporter_count: Some(spot.reporter_count),
                    is_notable: spot.is_notable,
                    notable_type: spot.notable_type.clone(),
                    confidence: Some(spot.confidence),
                    best_snr_network: spot.best_snr,
                    last_seen_network: Some(spot.last_seen),
                    // Network spots carry no passband / slot information.
                    audio_offset_hz: None,
                    slot_parity: None,
                });

            // If already exists from local decode, upgrade source
            if entry.source == SpotSource::Local {
                entry.source = SpotSource::Both;
            }
            // Always update network metadata
            entry.rarity_tier = Some(spot.rarity_tier.clone());
            entry.reporter_count = Some(spot.reporter_count);
            entry.is_notable = spot.is_notable;
            entry.notable_type = spot.notable_type.clone();
            entry.confidence = Some(spot.confidence);
            entry.best_snr_network = spot.best_snr;
            entry.last_seen_network = Some(spot.last_seen);
            // cqdx is authoritative for needs; keep them current each tick.
            entry.needed = spot.needed;
            entry.atno = spot.atno;
            if !spot.entity_name.is_empty() {
                entry.entity_name = Some(spot.entity_name.clone());
            }
            // Keep `last_seen` (the staleness clock for the DX Hunter display)
            // current with the network's last-heard timestamp so an actively
            // spotted station does not age off the list while it's still being
            // reported. Take the max so a fresh local decode is never aged back.
            if let Some(net_dt) = chrono::DateTime::from_timestamp(spot.last_seen, 0) {
                if net_dt > entry.last_seen {
                    entry.last_seen = net_dt;
                }
            }

            // Score the spot. `is_new_dxcc` / `is_new_band` aren't tracked
            // in the TUI (the coordinator owns the worked-set), so pass
            // false; rarity_tier is the dominant term for cluster spots
            // anyway. Keep the higher of any pre-existing local score and
            // the network score so a station heard both ways doesn't lose
            // rank on a merge tick.
            let net_score = crate::ui::dx_hunter::calculate_dx_priority(
                entry,
                &our_grid,
                entry.worked_before,
                false,
                false,
            );
            entry.priority_score = entry.priority_score.max(net_score);
        }

        // A network-spot merge can re-sort the DX-Hunter list; keep the cursor
        // pinned to the operator's selected callsign.
        self.clamp_dx_hunter_selection();
    }

    pub fn toggle_autonomous_pause(&mut self) {
        if let Some(ref mut status) = self.autonomous_status {
            if status.enabled {
                // Toggle paused state via the state string.
                if status.state == "Paused" {
                    status.state = "Hunting".to_string();
                    self.status_message = "Autonomous resumed".to_string();
                } else {
                    status.state = "Paused".to_string();
                    self.status_message = "Autonomous paused".to_string();
                }
                info!("Autonomous state: {}", status.state);
            }
        }
    }

    /// Find the best TX audio offset given current spectral activity and
    /// recent decode history. Returns `None` if every candidate is occupied
    /// in our parity. Caller updates `tx_frequency_offset` if Some.
    ///
    /// Scoring (lower = better):
    ///   spectral_penalty = peak amplitude in latest waterfall row near offset
    ///   decode_penalty   = N decodes within 37.5 Hz in our parity (last 60s)
    ///   own_penalty      = 1.0 if any active QSO within 75 Hz else 0.0
    ///   center_bias      = (|offset - 1500| / 1300) * 0.3   (small)
    ///
    /// Hard reject: any candidate with decode_penalty > 0 or own_penalty > 0.
    /// Among the remaining, lowest spectral + center_bias wins.
    /// Find the best TX audio offset. Returns `(offset_hz, is_truly_clear)`.
    /// Always returns `Some` when the search range is non-empty — on a busy
    /// band it picks the least-congested slot rather than giving up (a fully
    /// clear slot is rare in practice). `is_truly_clear` is false when the
    /// pick is a best-effort gap with stations within `SEPARATION_HZ`.
    pub fn find_clear_offset(&self) -> Option<(f64, bool)> {
        use pancetta_core::slot::SlotParity;

        const MIN_HZ: f64 = 200.0;
        const MAX_HZ: f64 = 2800.0;
        const STEP_HZ: f64 = 25.0;
        const SEPARATION_HZ: f64 = 75.0;
        const NEIGHBOR_HZ: f64 = 37.5;
        const SPECTRAL_RANGE_HZ: (f64, f64) = (0.0, 3000.0);

        let tx_parity: Option<SlotParity> = self.resolve_tx_parity();
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::seconds(60);

        let latest_row: Option<&Vec<f32>> = self.waterfall_data.last();

        let own_freqs: Vec<f64> = self.active_qsos.iter().map(|q| q.frequency_hz).collect();

        let recent_decodes_in_parity: Vec<f64> = self
            .decoded_messages
            .iter()
            .filter(|m| m.timestamp >= cutoff)
            .filter(|m| match (tx_parity, m.slot_parity) {
                (Some(my), Some(theirs)) => my == theirs,
                // tx_parity unknown → treat all decodes as blocking.
                (None, _) => true,
                // decode parity unknown → treat as blocking (safer default).
                (Some(_), None) => true,
            })
            .map(|m| m.delta_freq as f64)
            .collect();

        // Always return a best-effort offset: a fully clear slot is rare on a
        // live band, so instead of rejecting congested candidates we SCORE
        // every one and pick the least-congested. Tiered penalties guarantee
        // any truly-clear slot beats any congested slot, an occupied-but-
        // distant slot beats a nearer one, and our own running QSO is avoided
        // hardest. The returned bool is whether the chosen slot is truly clear
        // (no station within SEPARATION_HZ), for an honest status message.
        let mut best: Option<(f64, f64, bool)> = None; // (offset, score, is_clear)
        let mut hz = MIN_HZ;
        while hz <= MAX_HZ {
            let nearest_decode = recent_decodes_in_parity
                .iter()
                .map(|&f| (f - hz).abs())
                .fold(f64::INFINITY, f64::min);
            let nearest_own = own_freqs
                .iter()
                .map(|&f| (f - hz).abs())
                .fold(f64::INFINITY, f64::min);
            let near_decode = nearest_decode <= SEPARATION_HZ;
            let near_own = nearest_own <= SEPARATION_HZ;
            let is_clear = !near_decode && !near_own;

            let spectral = if let Some(row) = latest_row {
                spectral_peak(row, hz, NEIGHBOR_HZ, SPECTRAL_RANGE_HZ) as f64
            } else {
                0.0
            };
            let center_bias = ((hz - 1500.0).abs() / 1300.0) * 0.3;
            // Congestion penalties (base ensures tiering: clear < decode-near
            // < own-near; spectral ~[0,1] and center_bias ~[0,0.3] only break
            // ties within a tier). Graded by distance so we drift toward the
            // widest gap when nothing is clear.
            let decode_pen = if near_decode {
                10.0 + (SEPARATION_HZ - nearest_decode)
            } else {
                0.0
            };
            let own_pen = if near_own {
                100.0 + (SEPARATION_HZ - nearest_own)
            } else {
                0.0
            };
            let score = spectral + center_bias + decode_pen + own_pen;
            best = match best {
                Some((_, prev, _)) if prev <= score => best,
                _ => Some((hz, score, is_clear)),
            };
            hz += STEP_HZ;
        }
        best.map(|(hz, _, is_clear)| (hz, is_clear))
    }

    /// The parity our station will TX in. Active QSO wins; otherwise fall
    /// back to config (Even/Odd) or None for Auto.
    pub fn resolve_tx_parity(&self) -> Option<pancetta_core::slot::SlotParity> {
        if let Some(qso) = self.active_qsos.first() {
            if let Some(p) = qso.tx_parity {
                return Some(p);
            }
        }
        match self.config.station.tx_self_parity {
            pancetta_config::station::TxSelfParity::Even => {
                Some(pancetta_core::slot::SlotParity::Even)
            }
            pancetta_config::station::TxSelfParity::Odd => {
                Some(pancetta_core::slot::SlotParity::Odd)
            }
            pancetta_config::station::TxSelfParity::Auto => None,
        }
    }
}

/// A short human label for a reply step, for operator status messages
/// ("Replying 73 to VB7F"). Matches the vocabulary the Callers panel uses
/// (`ui::callers::reply_label`) but without a filled-in report value, since the
/// Space-feedback line names the action rather than the exact frame.
pub fn reply_step_label(step: pancetta_core::ResponseStep) -> &'static str {
    use pancetta_core::ResponseStep;
    match step {
        ResponseStep::Grid => "grid",
        ResponseStep::Report => "report",
        ResponseStep::ReportAck => "R-report",
        ResponseStep::Rr73 => "RR73",
        ResponseStep::SeventyThree => "73",
    }
}

/// Classify a station's directed-at-us message and pick the sequence step our
/// reply should open at. This is the Callers panel's smart default.
///
/// Mirrors `pancetta_qso::exchange::MessageExchange::parse_message`'s
/// classification, kept small and local because `pancetta-tui` does not depend
/// on `pancetta-qso`. The mapping (third token after two callsigns):
///
/// | their last message | our smart-default reply |
/// |--------------------|-------------------------|
/// | their CQ           | `Grid`                  |
/// | bare directed call (`US THEM`) | `Report`    |
/// | grid (`US THEM FN42`) | `Report`             |
/// | signal report (`US THEM -12`) | `ReportAck`  |
/// | R-report (`US THEM R-12`) | `Rr73`           |
/// | RR73 / RRR (`US THEM RR73`) | `SeventyThree` |
/// | 73 (`US THEM 73`)  | `SeventyThree`          |
/// | anything else      | `Grid`                  |
///
/// `our_call` is unused for the mapping itself (the message is already known
/// to be directed at us) but is accepted for symmetry and future use.
pub fn classify_caller_reply(msg_text: &str, _our_call: &str) -> pancetta_core::ResponseStep {
    use pancetta_core::ResponseStep;
    let text = msg_text.trim().to_uppercase();
    if text.is_empty() {
        return ResponseStep::Grid;
    }
    // Their CQ → start at the top with our grid.
    if text.starts_with("CQ ") || text == "CQ" {
        return ResponseStep::Grid;
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.as_slice() {
        // Bare directed call "US THEM" — no payload yet: send our report
        // (they are calling us and expect the report exchange to begin).
        [_to, _from] => ResponseStep::Report,
        // "US THEM <payload>"
        [_to, _from, payload] => {
            let p = *payload;
            if p == "RR73" || p == "RRR" || p == "73" {
                // They closed; we acknowledge with 73 (completes + logs).
                ResponseStep::SeventyThree
            } else if let Some(rest) = p.strip_prefix('R') {
                // "R-12" / "R+05" → they already rogered our report; close.
                // (Bare "R" with no sign falls through to Grid.)
                if rest.starts_with(['-', '+']) && rest[1..].chars().all(|c| c.is_ascii_digit()) {
                    ResponseStep::Rr73
                } else {
                    ResponseStep::Grid
                }
            } else if p.starts_with(['-', '+']) && p[1..].chars().all(|c| c.is_ascii_digit()) {
                // "-12" / "+05" → a signal report; reply with our R-report.
                ResponseStep::ReportAck
            } else if is_maidenhead_grid(p) {
                // A grid → they responded to our CQ; send our report.
                ResponseStep::Report
            } else {
                ResponseStep::Grid
            }
        }
        _ => ResponseStep::Grid,
    }
}

/// Count the unique callsigns that are calling US (directed-at-us decodes) in a
/// decoded-message snapshot — i.e. the number of rows the Callers panel would
/// show. Used to phrase the "restored N callers from Nm ago" status on a
/// band-return. Mirrors the de-dup in [`App::displayed_callers`].
fn directed_caller_count(messages: &VecDeque<DecodedMessageView>) -> usize {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in messages.iter() {
        if !msg.is_directed_at_us {
            continue;
        }
        if let Some(call) = msg.call_sign.as_ref() {
            if !call.is_empty() {
                seen.insert(call.to_uppercase());
            }
        }
    }
    seen.len()
}

/// `true` if `tok` looks like a 4- or 6-character Maidenhead grid.
fn is_maidenhead_grid(tok: &str) -> bool {
    let b = tok.as_bytes();
    if b.len() != 4 && b.len() != 6 {
        return false;
    }
    b[0].is_ascii_alphabetic()
        && b[1].is_ascii_alphabetic()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && (b.len() == 4 || (b[4].is_ascii_alphabetic() && b[5].is_ascii_alphabetic()))
}

/// Return the participant callsigns of a THIRD-PARTY exchange in `text`, or an
/// empty vec. A third-party exchange is a 3-token `<to> <from> <payload>`
/// message where the payload is a committed report/RR73/RRR/73 and neither
/// party is `our_call`. Mirrors
/// `pancetta_qso::autonomous::third_party_exchange_callsigns`.
fn third_party_exchange_participants(text: &str, our_call: &str) -> Vec<String> {
    let upper = text.trim().to_uppercase();
    if upper.starts_with("CQ ") || upper == "CQ" {
        return Vec::new();
    }
    let parts: Vec<&str> = upper.split_whitespace().collect();
    if parts.len() != 3 {
        return Vec::new();
    }
    let (to, from, payload) = (parts[0], parts[1], parts[2]);
    if !is_committed_exchange_payload(payload) {
        return Vec::new();
    }
    if to.eq_ignore_ascii_case(our_call) || from.eq_ignore_ascii_case(our_call) {
        return Vec::new();
    }
    let looks_like_call =
        |s: &str| s.len() >= 3 && s.chars().any(|c| c.is_ascii_digit()) && s != "73";
    let mut calls = Vec::new();
    if looks_like_call(to) {
        calls.push(to.to_string());
    }
    if looks_like_call(from) {
        calls.push(from.to_string());
    }
    calls
}

/// `true` if `tok` is the payload of a committed exchange (report / RR73 /
/// RRR / 73 / RR). Mirrors `pancetta_qso::autonomous::is_exchange_payload`.
fn is_committed_exchange_payload(tok: &str) -> bool {
    let u = tok.to_uppercase();
    if u == "RR73" || u == "RRR" || u == "73" || u == "RR" {
        return true;
    }
    let body = u.strip_prefix('R').unwrap_or(&u);
    if let Some(rest) = body.strip_prefix(['-', '+']) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Shared staleness window for rig S-meter readings, used by both
/// `App::s_meter_display` and `App::signal_strength_status_bar` so the two
/// displays (Station panel history / status bar) never disagree about when
/// a reading has gone stale.
const SIGNAL_STRENGTH_STALE_AFTER_SECS: i64 = 10;

/// Format a hamlib STRENGTH reading (dB relative to S9) as a
/// conventional S-meter string: "S0".."S9" below S9, "S9+NN" above.
/// One S-unit = 6 dB. Examples: -54 → "S0", -12 → "S7", 0 → "S9",
/// +20 → "S9+20".
pub fn format_s_meter(db_over_s9: i32) -> String {
    if db_over_s9 >= 0 {
        if db_over_s9 == 0 {
            "S9".to_string()
        } else {
            format!("S9+{}", db_over_s9)
        }
    } else {
        // 6 dB per S-unit below S9; clamp at S0 for very weak readings.
        let s_unit = (9 + db_over_s9.div_euclid(6)).max(0);
        format!("S{}", s_unit)
    }
}

/// Parse an operator-entered frequency in MHz (e.g. "14.085") to Hz. Returns
/// `None` for empty or malformed input. Rounds to the nearest Hz.
pub fn parse_mhz_to_hz(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let mhz: f64 = t.parse().ok()?;
    if !mhz.is_finite() || mhz <= 0.0 {
        return None;
    }
    Some((mhz * 1_000_000.0).round() as u64)
}

/// Interim US-band check: `true` when `tx_rf_hz` is outside the ham band ranges
/// modeled by `pancetta_core::Band::from_frequency` (used here as the proxy for
/// US bands). Region-aware band plans are a deferred TODO (see the split design
/// spec at docs/superpowers/specs/2026-06-25-arbitrary-freq-split-design.md).
pub fn tx_rf_out_of_us_band(tx_rf_hz: u64) -> bool {
    pancetta_core::Band::from_frequency(tx_rf_hz).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freq_modal_default_is_hidden_rxdial() {
        let m = super::FreqModalState::default();
        assert!(!m.visible);
        assert_eq!(m.field, super::FreqModalField::RxDial);
        assert!(m.rx_buffer.is_empty() && m.tx_buffer.is_empty());
    }

    /// With the test-only override unset, `tui_state_path()` must resolve
    /// to exactly `~/.pancetta/tui_state.json` — the real production path.
    /// This is a computation-only check (it never reads/writes the file
    /// itself), so it's safe to run without touching the operator's real
    /// state file.
    #[test]
    fn tui_state_path_default_is_real_home_pancetta_path() {
        // Explicitly clear any override a prior test on this thread may
        // have left behind, so this test is order-independent.
        TEST_TUI_STATE_PATH_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
        let expected = dirs::home_dir().map(|h| h.join(".pancetta").join("tui_state.json"));
        assert_eq!(App::tui_state_path(), expected);
    }

    fn fixture_view(call: &str, snr: i32) -> DecodedMessageView {
        DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            snr,
            delta_time: 0.0,
            delta_freq: 1500.0,
            call_sign: Some(call.to_string()),
            grid_square: None,
            message: format!("CQ {} FN42", call),
            distance: None,
            bearing: None,
            slot_parity: None,
            is_directed_at_us: false,
            worked_before: false,
            needed: false,
            atno: false,
            priority_score: None,
        }
    }

    fn fixture_view_directed(call: &str, snr: i32) -> DecodedMessageView {
        let mut v = fixture_view(call, snr);
        v.is_directed_at_us = true;
        v.message = format!("K5ARH {} -10", call);
        v
    }

    /// Band Activity fixture builder: `directed = false` produces a plain
    /// "CQ <call> ..." decode (via `fixture_view`); `directed = true`
    /// produces a directed-at-us decode (via `fixture_view_directed`).
    fn ba_fixture(call: &str, directed: bool) -> DecodedMessageView {
        if directed {
            fixture_view_directed(call, -10)
        } else {
            fixture_view(call, -10)
        }
    }

    async fn fixture_app() -> App {
        App::new(Config::default(), None)
            .await
            .expect("default Config should construct App")
    }

    /// Regression: highlighting a CQ at scroll>0, then a new decode arrives
    /// via add_decoded_message, must not silently change which logical
    /// message the operator's selector points at. Without the
    /// scroll-bump-on-insert in add_decoded_message, Space-press would
    /// call the wrong station every time a new slot's decodes landed.
    #[tokio::test]
    async fn add_decoded_message_preserves_highlight_on_arrival() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;

        app.add_decoded_message(fixture_view("OLDONE", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("MIDONE", -5))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("NEWONE", 0))
            .await
            .unwrap();

        // Reverse-order display: NEWONE (row 0), MIDONE (row 1), OLDONE (row 2).
        // Operator scrolls down twice to highlight OLDONE.
        app.band_activity_scroll = 2;
        let pre = app.get_selected_station().expect("OLDONE selectable");
        assert_eq!(pre.0, "OLDONE");

        // New decode lands a slot later.
        app.add_decoded_message(fixture_view("FRESH1", 3))
            .await
            .unwrap();

        // Highlight must STILL point at OLDONE — not at whatever happens
        // to be at row 2 after the prepend.
        let post = app.get_selected_station().expect("OLDONE still selectable");
        assert_eq!(
            post.0, "OLDONE",
            "highlight slid off-target after new decode arrived"
        );
    }

    /// When the operator has NOT scrolled (scroll == 0, tracking newest),
    /// add_decoded_message keeps them on row 0 — they want to see the
    /// freshest decode. Don't bump in this case.
    #[tokio::test]
    async fn add_decoded_message_keeps_tracking_newest_when_unscrolled() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;

        app.add_decoded_message(fixture_view("FIRST", -10))
            .await
            .unwrap();
        let pre = app.get_selected_station().expect("FIRST selectable");
        assert_eq!(pre.0, "FIRST");

        app.add_decoded_message(fixture_view("SECOND", -5))
            .await
            .unwrap();

        let post = app.get_selected_station().expect("SECOND selectable");
        assert_eq!(post.0, "SECOND");
        assert_eq!(app.band_activity_scroll, 0);
    }

    /// Directed-at-us decodes pin to the top of the displayed list,
    /// regardless of when they arrived relative to non-directed decodes.
    /// `get_selected_station` walks the same ordering, so scroll=0 lands
    /// on the directed message.
    #[tokio::test]
    async fn directed_decodes_pin_to_top_of_display() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;

        // Three plain decodes arrive first.
        app.add_decoded_message(fixture_view("PLAIN1", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("PLAIN2", -8))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("PLAIN3", -6))
            .await
            .unwrap();

        // Then a directed one arrives.
        app.add_decoded_message(fixture_view_directed("CALLER1", -4))
            .await
            .unwrap();

        // Display order: CALLER1 (directed, pinned top), then PLAIN3, PLAIN2,
        // PLAIN1 in newest-first.
        let displayed: Vec<&str> = app
            .displayed_messages()
            .iter()
            .filter_map(|m| m.call_sign.as_deref())
            .collect();
        assert_eq!(displayed, vec!["CALLER1", "PLAIN3", "PLAIN2", "PLAIN1"]);

        // band_activity_scroll was bumped from 0 → 1 by the prepend (CALLER1
        // is the new "row 0", so the previous track-newest position now
        // points one down). Move scroll back to 0 to highlight CALLER1.
        app.band_activity_scroll = 0;
        let selected = app.get_selected_station().expect("CALLER1 selectable");
        assert_eq!(selected.0, "CALLER1");
    }

    /// Hunt view narrows Band Activity to CQs-only among non-directed rows;
    /// directed-at-us rows are always kept regardless of view. This is the
    /// filter that drives the cursor, the renderer, and Space in Hunt view —
    /// it must live inside `displayed_messages()` so all three agree.
    #[tokio::test]
    async fn hunt_view_filters_band_activity_to_cqs_and_directed() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.add_decoded_message(ba_fixture("K1AAA", false))
            .await
            .unwrap(); // "CQ K1AAA EM12"
        let mut non_cq = ba_fixture("K2BBB", false);
        non_cq.message = "K9ZZZ K2BBB -10".into();
        app.add_decoded_message(non_cq).await.unwrap();
        let mut directed = ba_fixture("K3CCC", true);
        directed.message = "K5ARH K3CCC RR73".into();
        app.add_decoded_message(directed).await.unwrap();

        app.active_view = crate::view::ActiveView::Hunt;
        let shown: Vec<_> = app
            .displayed_messages()
            .iter()
            .filter_map(|m| m.call_sign.clone())
            .collect();
        assert!(shown.contains(&"K1AAA".to_string()), "CQ kept");
        assert!(
            shown.contains(&"K3CCC".to_string()),
            "directed-at-us always kept"
        );
        assert!(
            !shown.contains(&"K2BBB".to_string()),
            "third-party exchange filtered"
        );
    }

    /// Regression: in Hunt view `displayed_messages()` is shorter than the
    /// raw `decoded_messages` (non-CQ third-party rows are filtered out).
    /// `scroll_down`/`page_down`/`scroll_to_bottom` on the BandActivity panel
    /// must bound `band_activity_scroll` against the FILTERED length, not the
    /// raw decode count — otherwise the cursor can land on an index that's
    /// in-range for the raw list but out-of-range for the filtered one, and
    /// `pin_band_activity_selection()` silently drops the pin to `None`
    /// (`displayed_messages().get(band_activity_scroll)` misses), reintroducing
    /// the "cursor outlives a list mutation unpinned" bug class in a new guise.
    #[tokio::test]
    async fn hunt_view_scroll_clamps_to_filtered_length_not_raw() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        app.active_view = crate::view::ActiveView::Hunt;

        // 5 third-party (non-CQ) rows — filtered OUT of the Hunt view.
        for i in 0i32..5i32 {
            let mut m = fixture_view(&format!("K{i}NONCQ"), -10);
            m.message = format!("K9ZZZ K{i}NONCQ -10");
            app.add_decoded_message(m).await.unwrap();
        }
        // 2 CQ rows — kept in the Hunt view.
        app.add_decoded_message(fixture_view("K1CQA", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("K2CQB", -8))
            .await
            .unwrap();

        // Raw list has 7 rows; the Hunt-filtered list has only 2.
        assert_eq!(app.decoded_messages.len(), 7);
        assert_eq!(app.displayed_messages().len(), 2);

        // scroll_down repeatedly: with the bug, this walks past index 1
        // (bounded against the raw len of 7) instead of clamping at 1.
        for _ in 0..6 {
            app.scroll_down();
        }
        assert_eq!(
            app.band_activity_scroll, 1,
            "scroll_down must clamp to the filtered list's last index"
        );
        assert!(
            app.band_activity_pinned_call.is_some(),
            "scroll_down must not drop the pin by walking past the filtered list"
        );

        // page_down from a mid-filtered-list position must clamp the same way.
        app.band_activity_scroll = 0;
        app.pin_band_activity_selection();
        app.page_down();
        assert_eq!(
            app.band_activity_scroll, 1,
            "page_down must clamp to the filtered list's last index"
        );
        assert!(
            app.band_activity_pinned_call.is_some(),
            "page_down must not drop the pin by walking past the filtered list"
        );

        // scroll_to_bottom (End/G) must land on the filtered list's last row too.
        app.band_activity_scroll = 0;
        app.pin_band_activity_selection();
        app.scroll_to_bottom();
        assert_eq!(
            app.band_activity_scroll, 1,
            "scroll_to_bottom must jump to the filtered list's last index"
        );
        assert!(
            app.band_activity_pinned_call.is_some(),
            "scroll_to_bottom must not drop the pin by jumping past the filtered list"
        );
    }

    #[tokio::test]
    async fn quit_confirm_visible_defaults_false() {
        let app = App::new(Config::default(), None).await.unwrap();
        assert!(!app.quit_confirm_visible);
    }

    #[tokio::test]
    async fn resolves_parity_from_active_qso_when_present() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.active_qsos = vec![fixture_banner(
            "W1AW",
            "Calling",
            Some(pancetta_core::slot::SlotParity::Even),
        )];
        assert_eq!(
            app.resolve_tx_parity(),
            Some(pancetta_core::slot::SlotParity::Even)
        );
    }

    fn fixture_banner(
        call: &str,
        state: &str,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> ActiveQsoBanner {
        ActiveQsoBanner {
            their_callsign: call.into(),
            state: state.into(),
            started_at: chrono::Utc::now(),
            frequency_hz: 1234.0,
            tx_parity,
            last_tx_text: Some(format!("{} K5ARH EM10", call)),
            last_tx_at: Some(chrono::Utc::now()),
            last_rx_text: Some(format!("K5ARH {} -12", call)),
            last_rx_at: Some(chrono::Utc::now()),
            snr_rx: Some(-12),
            report_sent: Some(-8),
            report_received: Some(-15),
            exchange_count: 3,
            qso_id: format!("{}-id", call),
            initiated_by: "Manual".into(),
            ladder_labels: vec!["Grid".into(), "Rpt".into(), "R-Rpt".into()],
            ladder_ours: vec![true, false, true],
            ladder_index: 1,
            now_line: "waiting".into(),
            next_line: "their signal report".into(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
            dx_last_activity: None,
            hound: false,
        }
    }

    /// Batch 94: an active-QSOs snapshot populates the QSO-detail panel
    /// entries (qso_statuses) alongside the banner list.
    #[tokio::test]
    async fn apply_active_qsos_populates_detail_panel() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        assert!(app.qso_statuses.is_empty());

        app.apply_active_qsos(vec![fixture_banner("JA1ABC", "wait rpt", None)], Vec::new());

        assert_eq!(app.active_qsos.len(), 1);
        assert_eq!(app.qso_statuses.len(), 1);
        let q = &app.qso_statuses[0];
        assert!(q.active);
        assert_eq!(q.call_sign.as_deref(), Some("JA1ABC"));
        assert_eq!(q.state.as_deref(), Some("wait rpt"));
        assert_eq!(q.frequency, Some(1234.0));
        assert_eq!(q.last_tx_text.as_deref(), Some("JA1ABC K5ARH EM10"));
        assert_eq!(q.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert_eq!(q.report_sent, Some(-8));
        assert_eq!(q.report_received, Some(-15));
        // TX gauge = their report of us; RX gauge = measured RX SNR.
        assert_eq!(q.snr_tx, Some(-15));
        assert_eq!(q.snr_rx, Some(-12));
        assert_eq!(q.exchange_count, 3);
    }

    fn fixture_diag_event(text: &str) -> DiagnosticEventRecord {
        DiagnosticEventRecord {
            ts: Utc::now(),
            target: "qso",
            level: pancetta_core::DiagnosticLevel::Info,
            text: text.to_string(),
            qso_id: None,
            callsign: None,
        }
    }

    /// docs/observability-diagnostics-plan.md Layer 1: pushing events grows
    /// the retained history, and it's bounded (oldest evicted past the cap).
    #[tokio::test]
    async fn push_diagnostic_event_appends_and_bounds_history() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        assert!(app.diagnostic_events.is_empty());

        app.push_diagnostic_event(fixture_diag_event("first"));
        app.push_diagnostic_event(fixture_diag_event("second"));
        assert_eq!(app.diagnostic_events.len(), 2);
        assert_eq!(app.diagnostic_events[0].text, "first");
        assert_eq!(app.diagnostic_events[1].text, "second");

        for i in 0..600 {
            app.push_diagnostic_event(fixture_diag_event(&format!("event-{i}")));
        }
        assert!(
            app.diagnostic_events.len() <= 500,
            "diagnostic history must stay bounded, got {}",
            app.diagnostic_events.len()
        );
        // The oldest entries ("first"/"second") must have been evicted.
        assert!(!app.diagnostic_events.iter().any(|e| e.text == "first"));
    }

    /// The scroll cursor auto-follows new events when it was already at the
    /// newest entry (so a live event stream doesn't leave the operator
    /// staring at a stale position), but does NOT jump the cursor if the
    /// operator had scrolled back to review history.
    #[tokio::test]
    async fn push_diagnostic_event_auto_follows_only_when_at_the_end() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.push_diagnostic_event(fixture_diag_event("a"));
        app.push_diagnostic_event(fixture_diag_event("b"));
        assert_eq!(
            app.diagnostics_scroll, 1,
            "cursor starts at the newest entry"
        );

        app.push_diagnostic_event(fixture_diag_event("c"));
        assert_eq!(
            app.diagnostics_scroll, 2,
            "cursor auto-follows when it was at the end"
        );

        // Operator scrolls back to review history.
        app.diagnostics_scroll = 0;
        app.push_diagnostic_event(fixture_diag_event("d"));
        assert_eq!(
            app.diagnostics_scroll, 0,
            "cursor must NOT jump while the operator is scrolled back"
        );
    }

    /// Stale QSOs leave the detail panel when the next snapshot omits
    /// them — and an empty snapshot returns the panel to STANDBY.
    #[tokio::test]
    async fn apply_active_qsos_empty_snapshot_clears_panel() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.apply_active_qsos(vec![fixture_banner("JA1ABC", "wait rpt", None)], Vec::new());
        assert_eq!(app.qso_statuses.len(), 1);

        app.apply_active_qsos(Vec::new(), Vec::new());
        assert!(app.qso_statuses.is_empty());
        assert!(app.active_qsos.is_empty());
        assert!(!app.qso_status().active, "default entry is STANDBY");
    }

    /// Measured RX SNR missing → fall back to the report we sent.
    #[tokio::test]
    async fn apply_active_qsos_snr_rx_falls_back_to_report_sent() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        let mut banner = fixture_banner("JA1ABC", "wait rpt", None);
        banner.snr_rx = None;
        app.apply_active_qsos(vec![banner], Vec::new());
        assert_eq!(app.qso_statuses[0].snr_rx, Some(-8));
    }

    /// Batch 2 #1: the watchdog fields map from banner → QsoStatus.
    #[tokio::test]
    async fn apply_active_qsos_carries_watchdog_fields() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        let mut banner = fixture_banner("JA1ABC", "→ called", None);
        banner.call_count = 4;
        banner.max_calls = 10;
        let deadline = chrono::Utc::now() + chrono::Duration::minutes(5);
        banner.watchdog_deadline = Some(deadline);
        app.apply_active_qsos(vec![banner], Vec::new());
        let q = &app.qso_statuses[0];
        assert_eq!(q.call_count, 4);
        assert_eq!(q.max_calls, 10);
        assert_eq!(q.watchdog_deadline, Some(deadline));
    }

    /// Batch 2 #5: the QSO selection follows the SAME qso_id across snapshots
    /// even when row order/membership changes. Select the 2nd QSO, then push a
    /// new snapshot where a QSO was inserted at the front; the selection must
    /// still point at the originally-selected qso_id.
    #[tokio::test]
    async fn qso_cursor_pins_to_qso_id_across_snapshots() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.apply_active_qsos(
            vec![
                fixture_banner_id("AAA", "aaa-id"),
                fixture_banner_id("BBB", "bbb-id"),
            ],
            Vec::new(),
        );
        // Select BBB (index 1) and pin it.
        app.qso_cursor_down();
        assert_eq!(app.selected_qso_id().as_deref(), Some("bbb-id"));

        // New snapshot inserts CCC at the front → BBB moves to index 2.
        app.apply_active_qsos(
            vec![
                fixture_banner_id("CCC", "ccc-id"),
                fixture_banner_id("AAA", "aaa-id"),
                fixture_banner_id("BBB", "bbb-id"),
            ],
            Vec::new(),
        );
        assert_eq!(
            app.selected_qso_id().as_deref(),
            Some("bbb-id"),
            "selection must track the pinned qso_id, not the old position"
        );
    }

    /// When the pinned QSO disappears, the cursor falls back to a clamp and
    /// re-pins (no panic, points at a valid QSO).
    #[tokio::test]
    async fn qso_cursor_falls_back_when_pinned_qso_gone() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.apply_active_qsos(
            vec![
                fixture_banner_id("AAA", "aaa-id"),
                fixture_banner_id("BBB", "bbb-id"),
            ],
            Vec::new(),
        );
        app.qso_cursor_down(); // pin bbb-id
                               // bbb-id gone; only aaa remains.
        app.apply_active_qsos(vec![fixture_banner_id("AAA", "aaa-id")], Vec::new());
        assert_eq!(app.selected_qso_id().as_deref(), Some("aaa-id"));
    }

    fn fixture_banner_id(call: &str, qso_id: &str) -> ActiveQsoBanner {
        let mut b = fixture_banner(call, "wait rpt", None);
        b.qso_id = qso_id.to_string();
        b
    }

    #[tokio::test]
    async fn resolves_parity_from_config_when_idle() {
        let mut config = Config::default();
        config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
        let app = App::new(config, None).await.unwrap();
        assert_eq!(
            app.resolve_tx_parity(),
            Some(pancetta_core::slot::SlotParity::Even)
        );
    }

    #[tokio::test]
    async fn resolves_none_when_auto_and_idle() {
        let app = App::new(Config::default(), None).await.unwrap();
        // Default tx_self_parity is Auto.
        assert_eq!(app.resolve_tx_parity(), None);
    }

    /// Multiple directed decodes stack newest-first within the pinned
    /// region. Plain decodes follow in their own newest-first ordering.
    #[tokio::test]
    async fn directed_decodes_stack_newest_first_among_themselves() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;

        app.add_decoded_message(fixture_view_directed("DIR1", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("PLAIN1", -8))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view_directed("DIR2", -6))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("PLAIN2", -4))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view_directed("DIR3", -2))
            .await
            .unwrap();

        let displayed: Vec<&str> = app
            .displayed_messages()
            .iter()
            .filter_map(|m| m.call_sign.as_deref())
            .collect();
        // Pinned: DIR3, DIR2, DIR1 (newest-first); then PLAIN2, PLAIN1.
        assert_eq!(displayed, vec!["DIR3", "DIR2", "DIR1", "PLAIN2", "PLAIN1"]);
    }

    #[tokio::test]
    async fn find_clear_offset_avoids_busy_parity() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;

        let mut app = App::new(Config::default(), None).await.unwrap();
        // Set TX parity so the finder knows what to avoid.
        app.config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;

        // Saturate the band 1400-1600 Hz with Even-parity decodes (busy for us).
        let now = Utc::now();
        for f in (1400..1600).step_by(50) {
            app.decoded_messages.push_back(fixture_view_at(
                "AB1CD",
                f as f32,
                SlotParity::Even,
                now - Duration::seconds(5),
            ));
        }

        // Latest waterfall row is mostly quiet except 1400-1600.
        let mut row = vec![0.0f32; 100];
        for cell in &mut row[47..54] {
            *cell = 1.0;
        }
        app.waterfall_data.push(row);

        let (pick, is_clear) = app.find_clear_offset().expect("should find a clear spot");
        assert!(
            is_clear,
            "mostly-empty band should yield a truly clear slot"
        );
        // Should land outside 1400-1600 ± 75 Hz separation.
        assert!(
            !(1325.0..=1675.0).contains(&pick),
            "picked {} which is too close to busy band",
            pick
        );
        // Should be in the allowed range (200..2800).
        assert!((200.0..=2800.0).contains(&pick));
    }

    #[tokio::test]
    async fn find_clear_offset_best_effort_when_band_saturated() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;

        let mut app = App::new(Config::default(), None).await.unwrap();
        app.config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;

        // Decode every 25 Hz across the whole 200-2800 range in our parity.
        let now = Utc::now();
        for f in (200..=2800).step_by(25) {
            app.decoded_messages.push_back(fixture_view_at(
                "ZZZZZ",
                f as f32,
                SlotParity::Even,
                now - Duration::seconds(5),
            ));
        }

        // Even with the band saturated, t must still pick a best-effort
        // offset (never give up) — flagged as not-truly-clear.
        let (pick, is_clear) = app
            .find_clear_offset()
            .expect("must still return a best-effort offset on a busy band");
        assert!(!is_clear, "saturated band cannot yield a truly clear slot");
        assert!((200.0..=2800.0).contains(&pick));
    }

    #[tokio::test]
    async fn band_activity_jump_and_page_navigation() {
        use chrono::Utc;
        use pancetta_core::slot::SlotParity;

        let mut app = App::new(Config::default(), None).await.unwrap();
        app.active_panel = ActivePanel::BandActivity;
        let now = Utc::now();
        for i in 0..50 {
            app.decoded_messages.push_back(fixture_view_at(
                "Z",
                1000.0 + i as f32,
                SlotParity::Even,
                now,
            ));
        }

        // Scrolled deep into history, then Home snaps back to realtime (0).
        app.band_activity_scroll = 40;
        app.scroll_to_top();
        assert_eq!(app.band_activity_scroll, 0, "Home → newest/realtime");

        // End jumps to the oldest (len-1).
        app.scroll_to_bottom();
        assert_eq!(app.band_activity_scroll, 49, "End → oldest");

        // Page up moves toward realtime by a page; clamps at 0.
        app.page_up();
        assert_eq!(app.band_activity_scroll, 39);
        app.band_activity_scroll = 3;
        app.page_up();
        assert_eq!(app.band_activity_scroll, 0, "page up clamps at top");

        // Page down moves toward older; clamps at len-1.
        app.page_down();
        assert_eq!(app.band_activity_scroll, 10);
        app.band_activity_scroll = 45;
        app.page_down();
        assert_eq!(app.band_activity_scroll, 49, "page down clamps at bottom");
    }

    // Helper for the tests above.
    fn fixture_view_at(
        call: &str,
        delta_freq: f32,
        parity: pancetta_core::slot::SlotParity,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> DecodedMessageView {
        let mut v = fixture_view(call, -10);
        v.delta_freq = delta_freq;
        v.slot_parity = Some(parity);
        v.timestamp = timestamp;
        v
    }

    /// Batch 95: worked_before computed at the relay must flow through
    /// add_decoded_message into the DxStation entry the DX hunter
    /// renders (previously hardcoded false with a TODO).
    #[tokio::test]
    async fn add_decoded_message_carries_worked_before_into_dx_station() {
        let mut app = fixture_app().await;

        let mut worked = fixture_view("JA1ABC", -10);
        worked.worked_before = true;
        app.add_decoded_message(worked).await.unwrap();
        assert!(
            app.dx_stations.get("JA1ABC").unwrap().worked_before,
            "worked_before=true must reach the DX station entry"
        );

        let fresh = fixture_view("DL5XYZ", -10);
        app.add_decoded_message(fresh).await.unwrap();
        assert!(!app.dx_stations.get("DL5XYZ").unwrap().worked_before);
    }

    /// Pre-computed priority_score from the coordinator's real PriorityScorer
    /// must flow through `add_decoded_message` → `calculate_dx_priority` →
    /// `DxStation.priority_score` and produce DISTINCT scores for stations
    /// with different pre-computed values. When `priority_score: Some(v)`,
    /// `calculate_dx_priority` returns `v` directly (no coarse bucket rounding).
    /// Two stations with different scores must remain distinct in the DX Hunter.
    #[tokio::test]
    async fn pre_computed_priority_score_produces_nuanced_display() {
        let mut app = fixture_app().await;

        // Station A: pre-computed score 723 (e.g. needed DXCC + moderate SNR).
        let mut msg_a = fixture_view("JA1ABC", -12);
        msg_a.needed = true;
        msg_a.priority_score = Some(723);
        app.add_decoded_message(msg_a).await.unwrap();

        // Station B: pre-computed score 412 (e.g. rare but not needed + good SNR).
        let mut msg_b = fixture_view("W1XYZ", -5);
        msg_b.priority_score = Some(412);
        app.add_decoded_message(msg_b).await.unwrap();

        let score_a = app.dx_stations.get("JA1ABC").unwrap().priority_score;
        let score_b = app.dx_stations.get("W1XYZ").unwrap().priority_score;

        // Scores must be distinct (not collapsed to coarse 0/500/1000).
        assert_ne!(
            score_a, score_b,
            "pre-computed scores must produce distinct display values"
        );
        // And the values must exactly match what the coordinator set.
        assert_eq!(
            score_a, 723,
            "JA1ABC score must be exactly 723 (from scorer)"
        );
        assert_eq!(
            score_b, 412,
            "W1XYZ score must be exactly 412 (from scorer)"
        );
    }

    /// Fallback path (priority_score: None) still produces distinct scores via
    /// the SNR tiebreak fix — the full FT8 SNR range now contributes nuance.
    #[tokio::test]
    async fn fallback_snr_nuance_produces_distinct_scores() {
        let mut app = fixture_app().await;

        // Same station at two different SNR levels with no pre-computed score.
        let msg_weak = fixture_view("DL5XYZ", -20); // priority_score: None → fallback
        let msg_strong = fixture_view("G8KHF", 5); // priority_score: None → fallback

        app.add_decoded_message(msg_weak).await.unwrap();
        app.add_decoded_message(msg_strong).await.unwrap();

        let weak_score = app.dx_stations.get("DL5XYZ").unwrap().priority_score;
        let strong_score = app.dx_stations.get("G8KHF").unwrap().priority_score;

        assert!(
            strong_score > weak_score,
            "SNR +5 ({strong_score}) must rank above SNR -20 ({weak_score}) in fallback path"
        );
        assert_ne!(
            weak_score, strong_score,
            "fallback scores must not collapse to same value"
        );
    }

    /// #42: a self-decode of our own callsign must never appear as a DX entry.
    #[tokio::test]
    async fn self_decode_not_added_to_dx_hunter() {
        let mut app = fixture_app().await;
        app.station_info.call_sign = "K5ARH".to_string();

        // Our own call (any case) — must be skipped.
        app.add_decoded_message(fixture_view("K5ARH", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("k5arh", -10))
            .await
            .unwrap();
        assert!(
            !app.dx_stations.contains_key("K5ARH") && !app.dx_stations.contains_key("k5arh"),
            "must never list our own station in the DX Hunter"
        );

        // A real DX still gets added.
        app.add_decoded_message(fixture_view("JA1ABC", -10))
            .await
            .unwrap();
        assert!(app.dx_stations.contains_key("JA1ABC"));
    }

    /// #42 extension: add_dx_spot (PSKReporter/local path) must not insert our
    /// own callsign into dx_stations.
    #[tokio::test]
    async fn self_spot_not_added_via_add_dx_spot() {
        let mut app = fixture_app().await;
        app.station_info.call_sign = "K5ARH".to_string();

        // Our own call — must be rejected.
        app.add_dx_spot(
            "K5ARH".to_string(),
            14.074,
            "FT8".to_string(),
            -10,
            false,
            false,
            false,
        );
        app.add_dx_spot(
            "k5arh".to_string(),
            14.074,
            "FT8".to_string(),
            -10,
            false,
            false,
            false,
        );
        assert!(
            !app.dx_stations.contains_key("K5ARH") && !app.dx_stations.contains_key("k5arh"),
            "add_dx_spot must never insert our own callsign"
        );

        // A different station is still accepted.
        app.add_dx_spot(
            "JA1ABC".to_string(),
            14.074,
            "FT8".to_string(),
            -5,
            false,
            false,
            false,
        );
        assert!(app.dx_stations.contains_key("JA1ABC"));
    }

    /// #42 extension: merge_spot_groups (cqdx network path) must not insert our
    /// own callsign into dx_stations.
    #[test]
    fn self_spot_not_added_via_merge_spot_groups() {
        // merge_spot_groups is sync, so no async/await needed.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = rt.block_on(fixture_app());
        app.station_info.call_sign = "K5ARH".to_string();

        let self_spot = crate::tui_runner::CqdxSpotInfo {
            dx_call: "K5ARH".to_string(),
            band: "20m".to_string(),
            mode: "FT8".to_string(),
            frequency_hz: 14_074_000,
            grid: None,
            rarity_tier: "common".to_string(),
            reporter_count: 1,
            best_snr: Some(-10),
            confidence: 1.0,
            first_seen: 0,
            last_seen: chrono::Utc::now().timestamp(),
            is_notable: false,
            notable_type: None,
            entity_name: "United States".to_string(),
            needed: false,
            atno: false,
        };
        let other_spot = crate::tui_runner::CqdxSpotInfo {
            dx_call: "JA1ABC".to_string(),
            ..self_spot.clone()
        };

        app.merge_spot_groups(&[self_spot, other_spot]);

        assert!(
            !app.dx_stations.contains_key("K5ARH"),
            "merge_spot_groups must skip our own callsign"
        );
        assert!(
            app.dx_stations.contains_key("JA1ABC"),
            "merge_spot_groups must still insert other stations"
        );
    }

    /// #42 extension: displayed_dx_stations (render-time backstop) must not
    /// return our own callsign even if it somehow ended up in dx_stations.
    #[test]
    fn displayed_dx_stations_excludes_self() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = rt.block_on(fixture_app());
        app.station_info.call_sign = "K5ARH".to_string();

        // Inject our own call directly (bypassing the population guards).
        app.dx_stations.insert(
            "K5ARH".to_string(),
            DxStation {
                call_sign: "K5ARH".to_string(),
                grid_square: None,
                frequency: 14.074,
                mode: "FT8".to_string(),
                last_seen: chrono::Utc::now(),
                snr: -10,
                distance: None,
                bearing: None,
                worked_before: false,
                needed: false,
                atno: false,
                priority_score: 0,
                source: SpotSource::Local,
                entity_name: None,
                rarity_tier: None,
                reporter_count: None,
                is_notable: false,
                notable_type: None,
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
                audio_offset_hz: None,
                slot_parity: None,
            },
        );

        let displayed = app.displayed_dx_stations();
        assert!(
            displayed
                .iter()
                .all(|s| !s.call_sign.eq_ignore_ascii_case("K5ARH")),
            "displayed_dx_stations must never surface our own callsign"
        );
    }

    /// Batch 95 regression: an S-meter reading must NOT clobber the
    /// audio level gauge (the old update_signal_strength wrote the dB
    /// value into audio_level, a 0.0-1.0 RMS ratio).
    #[tokio::test]
    async fn s_meter_update_does_not_clobber_audio_level() {
        let mut app = fixture_app().await;
        app.audio_level = 0.42;
        app.update_signal_strength(-12);
        assert_eq!(app.audio_level, 0.42, "audio level must be untouched");
        assert_eq!(app.signal_strength_db, Some(-12));
        assert_eq!(app.s_meter_display().as_deref(), Some("S7"));
    }

    /// A stale S-meter reading (rig stopped reporting) renders as None
    /// so the UI shows "---" instead of a misleading frozen value.
    #[tokio::test]
    async fn s_meter_display_goes_stale() {
        let mut app = fixture_app().await;
        assert_eq!(app.s_meter_display(), None, "no reading yet");
        app.update_signal_strength(0);
        assert_eq!(app.s_meter_display().as_deref(), Some("S9"));
        // Backdate the reading past the staleness window.
        app.signal_strength_at = Some(Utc::now() - chrono::Duration::seconds(11));
        assert_eq!(app.s_meter_display(), None, "stale reading must hide");
    }

    /// Task 18 status-bar S-meter span: raw signed dB (NOT the S-unit
    /// conversion `s_meter_display` uses), same staleness gate/fields.
    #[tokio::test]
    async fn signal_strength_status_bar_reflects_reading_and_staleness() {
        let mut app = fixture_app().await;
        assert_eq!(app.signal_strength_status_bar(), "S:---", "no reading yet");
        app.update_signal_strength(20);
        assert_eq!(app.signal_strength_status_bar(), "S:+20");
        app.update_signal_strength(-12);
        assert_eq!(app.signal_strength_status_bar(), "S:-12");
        app.update_signal_strength(0);
        assert_eq!(
            app.signal_strength_status_bar(),
            "S:+0",
            "Rust's {{:+}} formats zero with an explicit sign"
        );
        // Backdate the reading past the shared staleness window.
        app.signal_strength_at = Some(Utc::now() - chrono::Duration::seconds(11));
        assert_eq!(
            app.signal_strength_status_bar(),
            "S:---",
            "stale reading must hide behind the same S:--- placeholder"
        );
    }

    /// hamlib STRENGTH convention: 0 dB = S9, 6 dB per S-unit below.
    #[test]
    fn format_s_meter_follows_hamlib_convention() {
        assert_eq!(format_s_meter(-54), "S0");
        assert_eq!(format_s_meter(-24), "S5");
        assert_eq!(format_s_meter(-12), "S7");
        assert_eq!(format_s_meter(-6), "S8");
        assert_eq!(format_s_meter(0), "S9");
        assert_eq!(format_s_meter(20), "S9+20");
        assert_eq!(format_s_meter(60), "S9+60");
        // Very weak readings clamp at S0 rather than going negative.
        assert_eq!(format_s_meter(-120), "S0");
    }

    // === Task A: DX Hunter unified sort + cursor ===

    fn dx_fixture(
        call: &str,
        priority: u32,
        snr: i32,
        freq: f64,
        worked_before: bool,
    ) -> DxStation {
        DxStation {
            call_sign: call.to_string(),
            grid_square: None,
            frequency: freq,
            mode: "FT8".to_string(),
            last_seen: Utc::now(),
            snr,
            distance: None,
            bearing: None,
            worked_before,
            needed: false,
            atno: false,
            priority_score: priority,
            source: SpotSource::Local,
            entity_name: None,
            rarity_tier: None,
            reporter_count: None,
            is_notable: false,
            notable_type: None,
            confidence: None,
            best_snr_network: None,
            last_seen_network: None,
            audio_offset_hz: Some(1200),
            slot_parity: None,
        }
    }

    /// The comparator must put needed (worked_before==false) stations first,
    /// then sort by priority desc, then snr desc, then freq asc, then call
    /// asc — a stable TOTAL order so the same set always renders the same way.
    #[tokio::test]
    async fn displayed_dx_stations_orders_needed_first_then_priority() {
        let mut app = fixture_app().await;
        // Worked station with very high priority should still sort AFTER
        // any needed station.
        app.dx_stations
            .insert("WORKED".into(), dx_fixture("WORKED", 999, 30, 14.074, true));
        app.dx_stations
            .insert("LOWPRI".into(), dx_fixture("LOWPRI", 10, 0, 14.074, false));
        app.dx_stations.insert(
            "HIGHPRI".into(),
            dx_fixture("HIGHPRI", 200, 5, 14.074, false),
        );

        let order: Vec<&str> = app
            .displayed_dx_stations()
            .iter()
            .map(|s| s.call_sign.as_str())
            .collect();
        assert_eq!(order, vec!["HIGHPRI", "LOWPRI", "WORKED"]);
    }

    /// A spot last heard beyond the staleness window drops off the displayed
    /// DX Hunter list, while a fresh one stays — even though both remain in the
    /// underlying `dx_stations` store (24 h retention).
    #[tokio::test]
    async fn displayed_dx_stations_drops_stale_entries() {
        let mut app = fixture_app().await;
        app.dx_stations
            .insert("FRESH".into(), dx_fixture("FRESH", 50, 10, 14.074, false));
        let mut stale = dx_fixture("STALE", 999, 30, 14.074, false);
        stale.last_seen = Utc::now() - chrono::Duration::seconds(App::DX_HUNTER_STALE_SECS + 5);
        app.dx_stations.insert("STALE".into(), stale);

        let shown: Vec<&str> = app
            .displayed_dx_stations()
            .iter()
            .map(|s| s.call_sign.as_str())
            .collect();
        assert_eq!(shown, vec!["FRESH"], "stale spot must not be displayed");
        // But it is still in the store (not yet 24h-cleaned).
        assert!(app.dx_stations.contains_key("STALE"));
    }

    /// Ties on priority break by snr desc, then freq asc, then call asc —
    /// deterministic regardless of HashMap iteration order.
    #[tokio::test]
    async fn displayed_dx_stations_tiebreak_is_deterministic() {
        let mut app = fixture_app().await;
        // Same priority + same snr + same freq → fall through to call asc.
        app.dx_stations
            .insert("ZZ9ZZ".into(), dx_fixture("ZZ9ZZ", 50, 10, 14.074, false));
        app.dx_stations
            .insert("AA1AA".into(), dx_fixture("AA1AA", 50, 10, 14.074, false));
        // Higher snr wins over the call tiebreak.
        app.dx_stations
            .insert("MM5MM".into(), dx_fixture("MM5MM", 50, 20, 14.074, false));

        let order: Vec<&str> = app
            .displayed_dx_stations()
            .iter()
            .map(|s| s.call_sign.as_str())
            .collect();
        assert_eq!(order, vec!["MM5MM", "AA1AA", "ZZ9ZZ"]);
    }

    /// The Space call-target (`get_selected_station`) MUST return the same
    /// station shown under the cursor — i.e. it walks the SAME order as
    /// `displayed_dx_stations`. This is the core bug Task A fixes (the old
    /// chooser sorted by last_seen, the renderer by priority).
    #[tokio::test]
    async fn get_selected_station_matches_displayed_cursor() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::DxHunter;
        app.dx_stations
            .insert("WORKED".into(), dx_fixture("WORKED", 999, 30, 14.074, true));
        app.dx_stations
            .insert("LOWPRI".into(), dx_fixture("LOWPRI", 10, 0, 14.075, false));
        app.dx_stations.insert(
            "HIGHPRI".into(),
            dx_fixture("HIGHPRI", 200, 5, 14.073, false),
        );

        let expected_order: Vec<String> = app
            .displayed_dx_stations()
            .iter()
            .map(|s| s.call_sign.clone())
            .collect();
        // For every cursor position, the selected callsign equals the
        // displayed row at that index.
        for (idx, expected) in expected_order.iter().enumerate() {
            app.dx_hunter_scroll = idx;
            let (call, _freq, _parity) = app.get_selected_station().expect("station at cursor");
            assert_eq!(&call, expected, "cursor {idx} must select the shown row");
        }
    }

    /// Local decodes carry the audio offset where we heard the station;
    /// network-only spots fall back to 1500 Hz. The Space target must
    /// preserve the local offset.
    #[tokio::test]
    async fn get_selected_station_preserves_local_audio_offset() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::DxHunter;
        let mut local = dx_fixture("LOCAL", 100, 10, 14.074, false);
        local.audio_offset_hz = Some(871);
        app.dx_stations.insert("LOCAL".into(), local);

        app.dx_hunter_scroll = 0;
        let (call, freq, _) = app.get_selected_station().unwrap();
        assert_eq!(call, "LOCAL");
        assert_eq!(freq, 871, "local audio offset must be preserved, not 1500");
    }

    #[tokio::test]
    async fn get_selected_station_network_spot_defaults_to_1500() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::DxHunter;
        let mut net = dx_fixture("NETONLY", 100, 10, 14.074, false);
        net.source = SpotSource::Network;
        net.audio_offset_hz = None; // network spots carry no passband info
        app.dx_stations.insert("NETONLY".into(), net);

        app.dx_hunter_scroll = 0;
        let (_, freq, _) = app.get_selected_station().unwrap();
        assert_eq!(freq, 1500, "network-only spot falls back to 1500 Hz");
    }

    /// A local re-decode of a callsign already known from the network must
    /// NOT blank the network metadata (rarity/reporter/notable). Before the
    /// fix, `add_decoded_message` did a blanket insert that wiped those.
    #[tokio::test]
    async fn add_decoded_message_preserves_network_metadata() {
        let mut app = fixture_app().await;
        // Seed a rich network entry.
        let mut net = dx_fixture("DX1ABC", 100, 5, 14.074, false);
        net.source = SpotSource::Network;
        net.rarity_tier = Some("legendary".to_string());
        net.reporter_count = Some(42);
        net.is_notable = true;
        app.dx_stations.insert("DX1ABC".into(), net);

        // Local re-decode of the same callsign.
        let view = fixture_view("DX1ABC", 12);
        app.add_decoded_message(view).await.unwrap();

        let entry = app.dx_stations.get("DX1ABC").unwrap();
        assert_eq!(
            entry.rarity_tier.as_deref(),
            Some("legendary"),
            "network rarity must survive a local re-decode"
        );
        assert_eq!(entry.reporter_count, Some(42));
        assert!(entry.is_notable);
        assert_eq!(entry.source, SpotSource::Both, "now heard both ways");
        assert_eq!(entry.snr, 12, "local snr updates");
        assert_eq!(
            entry.audio_offset_hz,
            Some(1500),
            "local audio offset captured"
        );
    }

    // === Task E: device picker state + config persistence ===

    #[tokio::test]
    async fn set_audio_devices_preselects_current_output() {
        let mut app = fixture_app().await;
        let input = vec![("Mic A".to_string(), true), ("Mic B".to_string(), false)];
        let output = vec![
            ("Speakers".to_string(), true),
            ("USB Codec".to_string(), false),
            ("HDMI".to_string(), false),
        ];
        app.set_audio_devices(input, output, Some("USB Codec".to_string()));
        assert_eq!(app.device_selection.selected_output_idx, 1);
        assert_eq!(
            app.device_selection.selected_output_name().as_deref(),
            Some("USB Codec")
        );
        // Input cursor defaults to the OS-default input.
        assert_eq!(app.device_selection.selected_input_idx, 0);
    }

    #[tokio::test]
    async fn set_audio_devices_default_falls_back_to_os_default() {
        let mut app = fixture_app().await;
        let output = vec![
            ("Speakers".to_string(), false),
            ("Default Out".to_string(), true),
        ];
        app.set_audio_devices(Vec::new(), output, Some("default".to_string()));
        assert_eq!(
            app.device_selection.selected_output_idx, 1,
            "'default' selects the OS-default entry"
        );
    }

    #[test]
    fn device_selection_nav_and_select_transitions() {
        let mut st = DeviceSelectionState::new();
        st.input_devices = vec![("In0".into(), true), ("In1".into(), false)];
        st.output_devices = vec![
            ("Out0".into(), true),
            ("Out1".into(), false),
            ("Out2".into(), false),
        ];

        // Starts on Input panel.
        assert_eq!(st.active_panel, DevicePanel::Input);
        st.move_down();
        assert_eq!(st.selected_input_idx, 1);
        st.move_down(); // clamp
        assert_eq!(st.selected_input_idx, 1);
        st.move_up();
        assert_eq!(st.selected_input_idx, 0);

        // Switch to Output and navigate independently.
        st.toggle_panel();
        assert_eq!(st.active_panel, DevicePanel::Output);
        st.move_down();
        st.move_down();
        assert_eq!(st.selected_output_idx, 2);
        assert_eq!(st.selected_output_name().as_deref(), Some("Out2"));
        // Input selection was untouched by output navigation.
        assert_eq!(st.selected_input_name().as_deref(), Some("In0"));
    }

    // === Callers panel ===================================================

    #[test]
    fn classify_caller_reply_smart_defaults() {
        use pancetta_core::ResponseStep as RS;
        let our = "K5ARH";
        // Their CQ → grid.
        assert_eq!(classify_caller_reply("CQ K9ZZ EM12", our), RS::Grid);
        // Bare directed call → report.
        assert_eq!(classify_caller_reply("K5ARH K9ZZ", our), RS::Report);
        // Grid reply → report.
        assert_eq!(classify_caller_reply("K5ARH K9ZZ FN42", our), RS::Report);
        // Signal report → report-ack.
        assert_eq!(classify_caller_reply("K5ARH K9ZZ -12", our), RS::ReportAck);
        assert_eq!(classify_caller_reply("K5ARH K9ZZ +05", our), RS::ReportAck);
        // R-report → RR73.
        assert_eq!(classify_caller_reply("K5ARH K9ZZ R-12", our), RS::Rr73);
        // Close tokens → 73.
        assert_eq!(
            classify_caller_reply("K5ARH K9ZZ RR73", our),
            RS::SeventyThree
        );
        assert_eq!(
            classify_caller_reply("K5ARH K9ZZ RRR", our),
            RS::SeventyThree
        );
        assert_eq!(
            classify_caller_reply("K5ARH K9ZZ 73", our),
            RS::SeventyThree
        );
        // Garbage → grid.
        assert_eq!(classify_caller_reply("???", our), RS::Grid);
    }

    #[test]
    fn third_party_exchange_participants_matches_qso_logic() {
        let our = "K5ARH";
        // Third-party exchange: both participants returned.
        let p = third_party_exchange_participants("JA1ABC W1XYZ -12", our);
        assert_eq!(p, vec!["JA1ABC".to_string(), "W1XYZ".to_string()]);
        // RR73 close also counts as an exchange.
        assert_eq!(
            third_party_exchange_participants("JA1ABC W1XYZ RR73", our).len(),
            2
        );
        // Grid reply is not yet a committed exchange.
        assert!(third_party_exchange_participants("JA1ABC W1XYZ FN42", our).is_empty());
        // CQ is not an exchange.
        assert!(third_party_exchange_participants("CQ JA1ABC PM95", our).is_empty());
        // Involving us → our own traffic, not third-party.
        assert!(third_party_exchange_participants("K5ARH JA1ABC -12", our).is_empty());
        assert!(third_party_exchange_participants("JA1ABC K5ARH R-12", our).is_empty());
    }

    #[tokio::test]
    async fn displayed_callers_dedups_newest_first() {
        let mut app = fixture_app().await;
        // Two directed-at-us decodes from K9ZZ (older then newer) + one from W1XYZ.
        let mut older = fixture_view_directed("K9ZZ", -10);
        older.timestamp = chrono::Utc::now() - chrono::Duration::seconds(30);
        older.message = "K5ARH K9ZZ -15".to_string();
        let newer = {
            let mut v = fixture_view_directed("K9ZZ", -5);
            v.message = "K5ARH K9ZZ R-10".to_string();
            v
        };
        let other = fixture_view_directed("W1XYZ", -8);
        let non_directed = fixture_view("NOTME", 0);

        app.add_decoded_message(older).await.unwrap();
        app.add_decoded_message(other).await.unwrap();
        app.add_decoded_message(newer).await.unwrap();
        app.add_decoded_message(non_directed).await.unwrap();

        let callers = app.displayed_callers();
        // Two unique callers (K9ZZ deduped), non-directed excluded.
        assert_eq!(callers.len(), 2);
        // K9ZZ's row is the NEWER decode (R-10), and it's newest-first.
        assert_eq!(callers[0].call_sign.as_deref(), Some("K9ZZ"));
        assert_eq!(callers[0].message, "K5ARH K9ZZ R-10");
    }

    #[tokio::test]
    async fn is_caller_busy_detects_third_party_exchange() {
        let mut app = fixture_app().await;
        app.station_info.call_sign = "K5ARH".to_string();
        // A third-party exchange decode (JA1ABC mid-QSO with W1XYZ).
        let mut tp = fixture_view("JA1ABC", -10);
        tp.message = "W1XYZ JA1ABC -12".to_string();
        app.add_decoded_message(tp).await.unwrap();
        assert!(app.is_caller_busy("JA1ABC"));
        assert!(app.is_caller_busy("W1XYZ"));
        assert!(!app.is_caller_busy("K9ZZ"));
    }

    // === UX audit Batch 1: callsign-pinned cursors ====================

    /// Selecting a DX-Hunter station then having a higher-priority spot
    /// arrive must keep the cursor on the operator's chosen callsign, not
    /// slide it to whatever sorts to the same row.
    #[tokio::test]
    async fn dx_hunter_cursor_pins_to_callsign_across_resort() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::DxHunter;

        // Two local decodes; both worked_before=false, same priority. Order is
        // by the stable comparator (snr desc then call asc). Add LOWPRI first.
        app.add_decoded_message(fixture_view("AA1AA", -15))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("BB2BB", -15))
            .await
            .unwrap();

        // Select whichever station is at row 1.
        let displayed = app.displayed_dx_stations();
        assert_eq!(displayed.len(), 2);
        let chosen = displayed[1].call_sign.clone();
        app.dx_hunter_scroll = 1;
        app.repin_dx_hunter();

        // A NEW higher-SNR station arrives — it sorts to the top, shifting rows.
        app.add_decoded_message(fixture_view("CC3CC", 10))
            .await
            .unwrap();

        // The cursor must still resolve to the operator's chosen callsign.
        let (sel, _, _) = app.get_selected_station().expect("a station selected");
        assert_eq!(sel, chosen, "DX-Hunter cursor must stay on pinned callsign");
    }

    /// Direct DX-Hunter analog of the Band Activity scroll-bump-on-push fix:
    /// with the operator scrolled DOWN (`dx_hunter_scroll > 0`), a new decode
    /// that sorts ABOVE the selection (shifting existing rows down by one)
    /// must not slide the highlight onto the freshly-arrived station. The
    /// callsign-pin mechanism re-derives `dx_hunter_scroll` from the pinned
    /// callsign after the list mutation, so the underlying scroll index
    /// advances in lock-step (the equivalent of Band Activity's `+= 1` bump),
    /// keeping the operator on the same logical row.
    #[tokio::test]
    async fn dx_hunter_scroll_index_tracks_pinned_row_on_new_top_arrival() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::DxHunter;

        // Three decodes, ascending SNR so display order (snr desc) is
        // CC3CC, BB2BB, AA1AA top-to-bottom.
        app.add_decoded_message(fixture_view("AA1AA", -15))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("BB2BB", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("CC3CC", -5))
            .await
            .unwrap();

        // Operator scrolls down to row 1 (BB2BB) and pins it.
        let displayed = app.displayed_dx_stations();
        assert_eq!(
            displayed
                .iter()
                .map(|s| s.call_sign.as_str())
                .collect::<Vec<_>>(),
            vec!["CC3CC", "BB2BB", "AA1AA"]
        );
        let chosen = displayed[1].call_sign.clone();
        assert_eq!(chosen, "BB2BB");
        app.dx_hunter_scroll = 1;
        app.repin_dx_hunter();

        // A NEW highest-SNR station arrives and sorts to the very top, pushing
        // every existing row (including the selection) down by one.
        app.add_decoded_message(fixture_view("ZZ9ZZ", 20))
            .await
            .unwrap();

        // The scroll index must have advanced 1 → 2 so it still points at the
        // pinned callsign — never at the freshly-arrived ZZ9ZZ at row 0.
        let displayed = app.displayed_dx_stations();
        assert_eq!(displayed[0].call_sign, "ZZ9ZZ", "new spot sorts to top");
        assert_eq!(
            app.dx_hunter_scroll, 2,
            "scroll index must bump to keep the highlight on the pinned row"
        );
        let (sel, _, _) = app.get_selected_station().expect("a station selected");
        assert_eq!(
            sel, chosen,
            "highlight stays on the operator's chosen station"
        );
    }

    /// A new caller arriving at row 0 must not shift the operator's selection
    /// off the caller they had pinned.
    #[tokio::test]
    async fn callers_cursor_pins_across_new_arrival() {
        let mut app = fixture_app().await;
        app.station_info.call_sign = "K5ARH".to_string();
        app.active_panel = ActivePanel::Callers;

        app.add_decoded_message(fixture_view_directed("AA1AA", -10))
            .await
            .unwrap();
        // Single caller selected at row 0.
        app.clamp_callers_selection();
        assert_eq!(
            app.selected_caller()
                .and_then(|m| m.call_sign.clone())
                .as_deref(),
            Some("AA1AA")
        );

        // A new caller arrives — newest-first puts it at row 0.
        app.add_decoded_message(fixture_view_directed("BB2BB", -8))
            .await
            .unwrap();

        // The selection must still be the originally-pinned caller.
        assert_eq!(
            app.selected_caller()
                .and_then(|m| m.call_sign.clone())
                .as_deref(),
            Some("AA1AA"),
            "Callers cursor must stay on pinned callsign across list growth"
        );
    }

    /// When the pinned caller leaves the list, the cursor clamps to bounds and
    /// the reply override resets (new pinned callsign).
    #[tokio::test]
    async fn callers_cursor_falls_back_when_pinned_call_gone() {
        let mut app = fixture_app().await;
        app.station_info.call_sign = "K5ARH".to_string();
        app.active_panel = ActivePanel::Callers;
        app.add_decoded_message(fixture_view_directed("AA1AA", -10))
            .await
            .unwrap();
        app.clamp_callers_selection();
        // Pin to AA1AA, set an override.
        app.caller_reply_override = Some(pancetta_core::ResponseStep::Rr73);
        app.caller_override_for = Some("AA1AA".to_string());

        // Wipe the list (simulating the caller aging out) and add a different one.
        app.decoded_messages.clear();
        app.add_decoded_message(fixture_view_directed("ZZ9ZZ", -5))
            .await
            .unwrap();

        // Cursor clamps; pin + override reset to the new caller's default.
        assert_eq!(
            app.selected_caller()
                .and_then(|m| m.call_sign.clone())
                .as_deref(),
            Some("ZZ9ZZ")
        );
        assert!(
            app.caller_reply_override.is_none(),
            "override resets when pinned callsign changes"
        );
    }

    // === Item 1: context-aware Space ======================================

    /// A directed-at-us decode from `call` carrying `payload` as the 3rd token
    /// (`<us> <them> <payload>`).
    fn fixture_view_directed_payload(call: &str, payload: &str, snr: i32) -> DecodedMessageView {
        let mut v = fixture_view(call, snr);
        v.is_directed_at_us = true;
        v.message = format!("K5ARH {} {}", call, payload);
        v
    }

    /// Space on a station that last sent us RR73 must REPLY at the 73 step
    /// (`SpaceAction::Reply { step: SeventyThree }`) — the core on-air bug fix
    /// (operator pressed Space to send 73, got their grid instead).
    #[tokio::test]
    async fn space_replies_seventy_three_to_rr73_sender() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        // VB7F repeatedly sending us RR73 (never copied our 73).
        app.add_decoded_message(fixture_view_directed_payload("VB7F", "RR73", -8))
            .await
            .unwrap();
        app.band_activity_scroll = 0;

        let action = app.resolve_space_action().expect("station selectable");
        match action {
            SpaceAction::Reply { callsign, step, .. } => {
                assert_eq!(callsign, "VB7F");
                assert_eq!(step, pancetta_core::ResponseStep::SeventyThree);
            }
            other => panic!("expected Reply(SeventyThree), got {:?}", other),
        }
    }

    /// Space on a pure CQer (nothing directed at us) keeps the historical
    /// behavior: `SpaceAction::Call` (answer their CQ with our grid).
    #[tokio::test]
    async fn space_calls_pure_cqer() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        // Only a CQ from this station — never directed at us.
        app.add_decoded_message(fixture_view("DL5XYZ", -3))
            .await
            .unwrap();
        app.band_activity_scroll = 0;

        let action = app.resolve_space_action().expect("station selectable");
        match action {
            SpaceAction::Call { callsign, .. } => assert_eq!(callsign, "DL5XYZ"),
            other => panic!("expected Call, got {:?}", other),
        }
    }

    /// Fix round 2 (operator-confirmed 2026-07-03): Space must NOT return a
    /// `Call` (fresh-initiation/supersede) for a station that already has a
    /// live QSO (`is_engaged`), even when the focus is resolved from a panel
    /// OTHER than QSO Status (here, Band Activity) — the guard is keyed off
    /// the callsign's engagement, not which panel it was focused from.
    /// `resolve_space_action` returns `None`, same as "nothing selected".
    #[tokio::test]
    async fn space_blocks_call_for_an_engaged_station_seen_in_band_activity() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        // DL5XYZ is calling CQ (nothing directed at us) but we already have a
        // live QSO with them (e.g. a concurrent multi-TX stream on another
        // slot) — Space here must not re-initiate/supersede it.
        app.add_decoded_message(fixture_view("DL5XYZ", -3))
            .await
            .unwrap();
        app.band_activity_scroll = 0;
        app.active_qsos = vec![fixture_banner("DL5XYZ", "Calling", None)];

        assert!(app.is_engaged("DL5XYZ"));
        assert!(app.last_directed_at_us_from("DL5XYZ").is_none());
        assert!(
            app.resolve_space_action().is_none(),
            "Space must be a no-op for an already-engaged station with no \
             pending directed-at-us reply"
        );
    }

    /// Space classifies each directed payload at the right rung: their grid →
    /// Report, their report → ReportAck, their R-report → Rr73, their RR73 → 73.
    #[tokio::test]
    async fn space_reply_step_tracks_their_last_message() {
        for (payload, expect) in [
            ("FN42", pancetta_core::ResponseStep::Report),
            ("-12", pancetta_core::ResponseStep::ReportAck),
            ("R-05", pancetta_core::ResponseStep::Rr73),
            ("73", pancetta_core::ResponseStep::SeventyThree),
        ] {
            let mut app = fixture_app().await;
            app.active_panel = ActivePanel::BandActivity;
            app.add_decoded_message(fixture_view_directed_payload("K9XYZ", payload, -10))
                .await
                .unwrap();
            app.band_activity_scroll = 0;
            match app.resolve_space_action().expect("selectable") {
                SpaceAction::Reply { step, .. } => assert_eq!(
                    step, expect,
                    "payload {payload:?} should classify to {expect:?}"
                ),
                other => panic!("payload {payload:?} expected Reply, got {other:?}"),
            }
        }
    }

    /// Space uses the MOST-RECENT directed message: an earlier report followed
    /// by a later RR73 from the same station resolves to the 73 step.
    #[tokio::test]
    async fn space_uses_most_recent_directed_message() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        app.add_decoded_message(fixture_view_directed_payload("VB7F", "-10", -8))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view_directed_payload("VB7F", "RR73", -8))
            .await
            .unwrap();
        app.band_activity_scroll = 0;
        match app.resolve_space_action().expect("selectable") {
            SpaceAction::Reply { step, .. } => {
                assert_eq!(step, pancetta_core::ResponseStep::SeventyThree)
            }
            other => panic!("expected Reply(SeventyThree), got {other:?}"),
        }
    }

    // === Item 3: band-switch clear + 10-min restore =======================

    /// A band switch clears the Band Activity, Callers (both derive from
    /// `decoded_messages`) and DX Hunter lists, and resets their cursors.
    #[tokio::test]
    async fn band_switch_clears_all_lists() {
        let mut app = fixture_app().await;
        app.add_decoded_message(fixture_view_directed("AA1AA", -5))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("BB2BB", -3))
            .await
            .unwrap();
        app.dx_hunter_scroll = 1;
        app.callers_scroll = 0;
        assert!(!app.decoded_messages.is_empty());
        assert!(!app.dx_stations.is_empty());

        app.band_up();

        assert!(app.decoded_messages.is_empty(), "decodes cleared");
        assert!(app.dx_stations.is_empty(), "DX list cleared");
        assert!(app.displayed_callers().is_empty(), "Callers cleared");
        assert_eq!(app.band_activity_scroll, 0);
        assert_eq!(app.dx_hunter_scroll, 0);
        assert_eq!(app.callers_scroll, 0);
    }

    /// Returning to a band within the 10-minute TTL restores its Callers (and
    /// DX) list and surfaces a "restored N callers" status.
    #[tokio::test]
    async fn band_return_within_ttl_restores_callers() {
        let mut app = fixture_app().await;
        app.add_decoded_message(fixture_view_directed("AA1AA", -5))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view_directed("CC3CC", -7))
            .await
            .unwrap();
        assert_eq!(app.displayed_callers().len(), 2);

        app.band_up(); // leave 20M (snapshot stashed), lists cleared
        assert!(app.displayed_callers().is_empty());

        app.band_down(); // back to 20M within TTL → restore

        assert_eq!(
            app.displayed_callers().len(),
            2,
            "callers restored on return within TTL"
        );
        assert!(
            app.status_message.contains("restored") && app.status_message.contains("2 caller"),
            "restore should be visible in status: {:?}",
            app.status_message
        );
    }

    /// A snapshot older than the 10-minute TTL is dropped — returning to that
    /// band starts fresh (no restore).
    #[tokio::test]
    async fn band_return_after_ttl_starts_fresh() {
        let mut app = fixture_app().await;
        app.add_decoded_message(fixture_view_directed("AA1AA", -5))
            .await
            .unwrap();

        app.band_up(); // stash 20M snapshot

        // Age the stashed snapshot past the TTL.
        let twenty_m = app.config.bands.bands[5].name.clone();
        assert_eq!(twenty_m, "20M");
        if let Some(snap) = app.band_cache.get_mut(&twenty_m) {
            snap.captured_at =
                Utc::now() - chrono::Duration::seconds(App::BAND_CACHE_TTL_SECS + 60);
        } else {
            panic!("20M snapshot should have been cached");
        }

        app.band_down(); // back to 20M, but snapshot is stale

        assert!(
            app.displayed_callers().is_empty(),
            "stale snapshot must not restore"
        );
        assert!(
            !app.status_message.contains("restored"),
            "no restore status for stale snapshot: {:?}",
            app.status_message
        );
    }

    /// The band cache is bounded: hopping across more than `BAND_CACHE_MAX`
    /// bands never grows the cache past the cap.
    #[tokio::test]
    async fn band_cache_is_bounded() {
        let mut app = fixture_app().await;
        for _ in 0..(App::BAND_CACHE_MAX + 4) {
            // Each band gets a decode so a snapshot is actually stashed.
            app.add_decoded_message(fixture_view_directed("AA1AA", -5))
                .await
                .unwrap();
            app.band_up();
        }
        assert!(
            app.band_cache.len() <= App::BAND_CACHE_MAX,
            "band cache exceeded cap: {}",
            app.band_cache.len()
        );
    }

    #[test]
    fn parse_mhz_to_hz_accepts_and_rejects() {
        assert_eq!(super::parse_mhz_to_hz("14.085"), Some(14_085_000));
        assert_eq!(super::parse_mhz_to_hz("7.074"), Some(7_074_000));
        assert_eq!(super::parse_mhz_to_hz("14"), Some(14_000_000));
        assert_eq!(super::parse_mhz_to_hz("  14.085  "), Some(14_085_000));
        assert_eq!(super::parse_mhz_to_hz(""), None);
        assert_eq!(super::parse_mhz_to_hz("abc"), None);
        assert_eq!(super::parse_mhz_to_hz("14.0.0"), None);
        assert_eq!(super::parse_mhz_to_hz("-5"), None);
    }

    #[test]
    fn tx_rf_out_of_us_band_flags_only_out_of_band() {
        assert!(!super::tx_rf_out_of_us_band(14_074_000 + 1_500)); // 20m
        assert!(!super::tx_rf_out_of_us_band(28_500_000)); // 10m
        assert!(super::tx_rf_out_of_us_band(15_000_000)); // between 20m and 17m
        assert!(super::tx_rf_out_of_us_band(100_000_000)); // nowhere near a ham band
    }

    #[test]
    fn freq_modal_buffers_parse_simplex_and_split() {
        let mut m = super::FreqModalState {
            visible: true,
            ..Default::default()
        };
        m.rx_buffer.push_str("14.085");
        assert_eq!(super::parse_mhz_to_hz(&m.rx_buffer), Some(14_085_000));
        // empty tx buffer = simplex
        assert_eq!(super::parse_mhz_to_hz(&m.tx_buffer), None);
        m.tx_buffer.push_str("14.090");
        assert_eq!(super::parse_mhz_to_hz(&m.tx_buffer), Some(14_090_000));
    }

    #[tokio::test]
    async fn band_activity_cursor_follows_callsign_across_reorder() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        app.add_decoded_message(fixture_view("K1AAA", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("K2BBB", -8))
            .await
            .unwrap();
        // Newest-first display: [K2BBB, K1AAA]. Move cursor to K1AAA (row 1) and pin.
        app.scroll_down();
        app.pin_band_activity_selection();
        let (call, _, _) = app.get_selected_station().expect("K1AAA selectable");
        assert_eq!(call, "K1AAA");

        // A new decode lands (K3CCC becomes row 0) AND a directed-at-us decode
        // pins to the very top — both shift K1AAA's index.
        app.add_decoded_message(fixture_view("K3CCC", -6))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view_directed("K4DDD", -4))
            .await
            .unwrap();

        // The cursor must still resolve to K1AAA, not whatever occupies row 1 now.
        let (call, _, _) = app.get_selected_station().expect("K1AAA still selectable");
        assert_eq!(call, "K1AAA");
    }

    #[tokio::test]
    async fn band_activity_pin_degrades_gracefully_when_station_ages_out() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        app.add_decoded_message(fixture_view("K1AAA", -10))
            .await
            .unwrap();
        app.pin_band_activity_selection();
        app.clear_messages();
        // Pinned call gone: index clamps to 0, pin cleared, no panic.
        assert_eq!(app.band_activity_scroll, 0);
    }

    #[tokio::test]
    async fn band_activity_pageup_pagedown_pin_selection() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        // Add 15 messages so PageUp/PageDown (10-row jumps) have room to move.
        for i in 0i32..15i32 {
            app.add_decoded_message(fixture_view(&format!("K{:02}XXX", i), -20 + i))
                .await
                .unwrap();
        }
        // Newest-first: [K14XXX, K13XXX, ..., K0XXX]. Move to row 5 and page down.
        app.band_activity_scroll = 5;
        app.page_down();
        // After page_down, we moved down 10 rows. The key is that band_activity_pinned_call
        // should now be set to whatever station is at the new scroll position.
        let (pinned_call_after, _, _) =
            app.get_selected_station().expect("station after page_down");
        assert_eq!(
            app.band_activity_pinned_call,
            Some(pinned_call_after.clone()),
            "PageDown should pin to the new selection"
        );
        // Now add a high-priority directed-at-us decode that would normally shift indices.
        // Without the fix, the pin would be stale and the cursor would snap to the old position.
        app.add_decoded_message(fixture_view_directed("K99NEW", -2))
            .await
            .unwrap();
        // Verify cursor stayed on the same pinned callsign despite the reorder.
        let (current_call, _, _) = app.get_selected_station().expect("station after reorder");
        assert_eq!(
            current_call, pinned_call_after,
            "PageDown pin should survive a reorder"
        );
    }

    #[tokio::test]
    async fn band_activity_pin_self_heals_when_station_ages_out() {
        let mut app = fixture_app().await;
        app.active_panel = ActivePanel::BandActivity;
        // Add three stations so we have backup after removing the pinned one
        app.add_decoded_message(fixture_view("K1AAA", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("K2BBB", -12))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("K3CCC", -14))
            .await
            .unwrap();
        // Newest-first: [K3CCC, K2BBB, K1AAA]
        // Pin K2BBB (the middle station)
        app.band_activity_scroll = 1;
        app.pin_band_activity_selection();
        assert_eq!(
            app.band_activity_pinned_call,
            Some("K2BBB".to_string()),
            "Should pin to K2BBB at index 1"
        );
        // Manually remove K2BBB from the list to simulate it aging out
        // (without clearing all messages)
        app.decoded_messages
            .retain(|m| m.call_sign.as_deref() != Some("K2BBB"));
        // Displayed list is now [K3CCC, K1AAA]
        app.clamp_band_activity_selection();
        // After clamp with K2BBB gone:
        // - K2BBB is not found, so pinned_call is cleared
        // - Scroll was 1, now clamped to 1 (max index in a 2-element list)
        // - Re-adoption should set pinned_call to whatever is at index 1, which is K1AAA
        assert_eq!(
            app.band_activity_pinned_call,
            Some("K1AAA".to_string()),
            "Pin should self-heal to K1AAA at the resolved scroll index"
        );
        // Now add a new high-priority decode that reorders the list
        app.add_decoded_message(fixture_view_directed("K4DDD", -2))
            .await
            .unwrap();
        // Displayed list is now [K4DDD (directed), K3CCC, K1AAA]
        // The self-healed pin to K1AAA should survive this reorder
        assert_eq!(
            app.band_activity_pinned_call,
            Some("K1AAA".to_string()),
            "Self-healed pin should survive a reorder"
        );
        // Verify cursor is still on K1AAA despite the new decode at index 0
        let (current_call, _, _) = app
            .get_selected_station()
            .expect("station after re-adopt reorder");
        assert_eq!(
            current_call, "K1AAA",
            "Cursor should still resolve to K1AAA after reorder"
        );
    }

    #[tokio::test]
    async fn focused_callsign_follows_the_active_panel() {
        let mut app = fixture_app().await;
        app.add_decoded_message(fixture_view("K1AAA", -10))
            .await
            .unwrap();
        app.active_panel = ActivePanel::BandActivity;
        app.pin_band_activity_selection();
        assert_eq!(app.focused_callsign().as_deref(), Some("K1AAA"));
        // Compound equivalence: EA8/K1AAA is the same focus target.
        assert!(app.is_focused("EA8/K1AAA"));
        assert!(!app.is_focused("K1AAB"));
    }

    #[tokio::test]
    async fn engaged_calls_are_flagged_compound_aware() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.active_qsos
            .push(fixture_banner("JA1ABC", "wait rpt", None));
        assert!(app.is_engaged("JA1ABC"));
        assert!(app.is_engaged("JA1ABC/P"));
        assert!(!app.is_engaged("JA1ABD"));
    }

    #[tokio::test]
    async fn placement_update_clamps_cursor() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.placement_cursor = 7;
        app.apply_placement(PlacementView {
            slices: vec![PlacementSlice {
                offset_hz: 1480.0,
                score: 98.0,
                clear_first: true,
                clear_second: true,
            }],
            openness: vec![3; 96],
            bin_hz: 25.0,
            range: (200.0, 2600.0),
            received_at: Utc::now(),
        });
        assert_eq!(app.placement_cursor, 0);
    }

    /// Build a 96-bin PlacementView (matching `range`/`bin_hz` used
    /// elsewhere in this test module) with every bin at code 3 (both
    /// clear) except `parked_bin`, which is set to `code`.
    fn placement_view_with_bin_code(parked_bin: usize, code: u8) -> PlacementView {
        let mut openness = vec![3u8; 96];
        openness[parked_bin] = code;
        PlacementView {
            slices: vec![PlacementSlice {
                offset_hz: 900.0,
                score: 80.0,
                clear_first: true,
                clear_second: true,
            }],
            openness,
            bin_hz: 25.0,
            range: (200.0, 2600.0),
            received_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn park_degradation_warning_fires_once_on_edge_transition() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        // Park at 1500 Hz -> bin (1500-200)/25 = 52.
        app.tx_offset_hold_hz = Some(1500);

        // First snapshot after parking: bin is fully clear (code 3). No
        // prior baseline (`park_coverage_last` starts `None`), so this
        // must NOT fire — it only establishes the baseline.
        app.apply_placement(placement_view_with_bin_code(52, 3));
        assert_eq!(app.park_coverage_last, Some((1500, 3)));
        assert!(
            !app.status_message.contains("now busy"),
            "no warning on the baseline-establishing snapshot, got: {}",
            app.status_message
        );

        // Coverage degrades 3 -> 1 (strict decrease): fires exactly once.
        app.apply_placement(placement_view_with_bin_code(52, 1));
        assert!(
            app.status_message.contains("now busy"),
            "expected a degradation warning, got: {}",
            app.status_message
        );
        assert_eq!(app.park_coverage_last, Some((1500, 1)));

        // Stash a sentinel, then apply an UNCHANGED (still code 1) snapshot.
        // Edge-triggered: must NOT re-fire / rewrite status_message.
        app.status_message = "SENTINEL".to_string();
        app.apply_placement(placement_view_with_bin_code(52, 1));
        assert_eq!(
            app.status_message, "SENTINEL",
            "warning must not repeat while still degraded (edge-triggered, not per-update)"
        );
        assert_eq!(app.park_coverage_last, Some((1500, 1)));
    }

    #[tokio::test]
    async fn park_degradation_warning_does_not_fire_on_improvement_or_lateral_move() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.tx_offset_hold_hz = Some(1500);

        // Establish baseline at code 1.
        app.apply_placement(placement_view_with_bin_code(52, 1));
        assert_eq!(app.park_coverage_last, Some((1500, 1)));

        // Improves 1 -> 2: no warning.
        app.status_message = "SENTINEL".to_string();
        app.apply_placement(placement_view_with_bin_code(52, 2));
        assert_eq!(
            app.status_message, "SENTINEL",
            "an improvement must not trigger the degradation warning"
        );
        assert_eq!(app.park_coverage_last, Some((1500, 2)));
    }

    #[tokio::test]
    async fn park_coverage_last_resets_to_none_when_not_parked() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.tx_offset_hold_hz = Some(1500);
        app.apply_placement(placement_view_with_bin_code(52, 3));
        assert_eq!(app.park_coverage_last, Some((1500, 3)));

        app.tx_offset_hold_hz = None;
        app.apply_placement(placement_view_with_bin_code(52, 3));
        assert_eq!(
            app.park_coverage_last, None,
            "no park active -> baseline resets to None"
        );
    }

    /// Fix round 1 regression: re-parking to a DIFFERENT frequency between
    /// two `apply_placement` calls (entirely plausible — the operator can
    /// re-park faster than the ~15s FT8-window `PlacementView` cadence)
    /// must NOT compare the new frequency's code against the old
    /// frequency's stale baseline. Here bin 52 (freq A, 1500 Hz) degrades
    /// 3->1 while parked, matching the genuine-degradation test above — but
    /// the operator then re-parks to bin 10 (freq B, 450 Hz) BEFORE the
    /// next snapshot arrives, and B's code hasn't moved at all (stays at
    /// 3 across the re-park). No warning should fire on that next
    /// `apply_placement`, because the tracked baseline was for A, not B.
    #[tokio::test]
    async fn re_park_to_different_frequency_does_not_compare_against_stale_baseline() {
        let mut app = App::new(Config::default(), None).await.unwrap();

        // Park at freq A (1500 Hz -> bin 52), establish baseline code 3.
        app.tx_offset_hold_hz = Some(1500);
        app.apply_placement(placement_view_with_bin_code(52, 3));
        assert_eq!(app.park_coverage_last, Some((1500, 3)));

        // Re-park to a DIFFERENT frequency B (450 Hz -> bin 10) WITHOUT an
        // intervening apply_placement call at A — exactly the ordinary
        // "oops, wrong slice" interaction the operator can trigger via
        // Enter-park or the `o` modal faster than a snapshot arrives.
        app.tx_offset_hold_hz = Some(450);

        // Next snapshot: B's bin (10) is untouched (still code 3, i.e. no
        // real degradation happened at B at all). A stale-baseline bug
        // would compare B's code 3 against A's leftover baseline code 3 —
        // which wouldn't spuriously fire here since they're equal, so
        // additionally verify the baseline correctly rebases to B (not
        // left pointing at A) and no warning fires.
        app.status_message = "SENTINEL".to_string();
        app.apply_placement(placement_view_with_bin_code(10, 3));
        assert_eq!(
            app.status_message, "SENTINEL",
            "no real degradation at the newly-parked frequency -> no warning"
        );
        assert_eq!(
            app.park_coverage_last,
            Some((450, 3)),
            "baseline must rebase to the newly-parked frequency, not stay on the old one"
        );

        // Now degrade B genuinely (3 -> 1) at the SAME frequency: this
        // must warn, proving the rebase above established a real usable
        // baseline (not just permanently suppressed).
        app.apply_placement(placement_view_with_bin_code(10, 1));
        assert!(
            app.status_message.contains("now busy"),
            "a genuine degradation at the (now-current) parked frequency must still warn, got: {}",
            app.status_message
        );
        assert_eq!(app.park_coverage_last, Some((450, 1)));
    }

    /// Fix round 1 regression, the sharper case — this is the scenario that
    /// would actually fire a SPURIOUS warning under the pre-fix code: park
    /// at freq A with a HIGH tracked code (3), re-park to freq B whose OWN
    /// code has always been LOWER (1, unrelated to A, no real change at
    /// B), then apply a snapshot where B's code is unchanged. Comparing B's
    /// code (1) against A's stale baseline (3) is a strict decrease
    /// (1 < 3) and would incorrectly fire "now busy" purely from the
    /// frequency swap, even though nothing degraded at B at all.
    #[tokio::test]
    async fn re_park_to_different_frequency_with_worse_baseline_does_not_spuriously_warn() {
        let mut app = App::new(Config::default(), None).await.unwrap();

        // Park at freq A (1500 Hz -> bin 52); baseline established at the
        // BEST code (3) — this is the value a stale comparison would use.
        app.tx_offset_hold_hz = Some(1500);
        app.apply_placement(placement_view_with_bin_code(52, 3));
        assert_eq!(app.park_coverage_last, Some((1500, 3)));

        // Re-park to freq B (450 Hz -> bin 10), which has ALREADY been at
        // code 1 the whole time (unrelated to A) — no real degradation
        // occurs at B between this and the next snapshot.
        app.tx_offset_hold_hz = Some(450);
        app.status_message = "SENTINEL".to_string();
        app.apply_placement(placement_view_with_bin_code(10, 1));

        // The fix: B's fresh code (1) must NOT be compared against A's
        // stale baseline (3) — that comparison (1 < 3) would spuriously
        // fire. Since park_coverage_last was tracking A's frequency, not
        // B's, this must be treated as a fresh baseline instead.
        assert_eq!(
            app.status_message, "SENTINEL",
            "must not compare B's code against A's stale baseline across a re-park"
        );
        assert_eq!(
            app.park_coverage_last,
            Some((450, 1)),
            "baseline must rebase to the newly-parked frequency B, not stay on A"
        );
    }

    // === Task 19: mouse click-to-focus / click-to-park =====================

    fn left_click(x: u16, y: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// Both the click handler and the test derive the panel's rect from
    /// the SAME `ui::view_rects` the renderer draws from — so this proves
    /// the click lands on the boundary the operator actually sees, not
    /// just on whatever coordinates the test happened to hand-compute.
    fn operate_panel_rect(app: &App, panel: ActivePanel) -> ratatui::layout::Rect {
        let content_area =
            crate::ui::content_area(Rect::new(0, 0, app.terminal_size.0, app.terminal_size.1));
        crate::ui::view_rects(app.active_view, app.zoomed, app.active_panel, content_area)
            .into_iter()
            .find(|(p, _)| *p == panel)
            .map(|(_, r)| r)
            .unwrap_or_else(|| panic!("{:?} not present in this view", panel))
    }

    #[tokio::test]
    async fn click_band_activity_row_focuses_and_pins_it() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::DxHunter; // start on a different panel
        app.add_decoded_message(fixture_view("K1AAA", -10))
            .await
            .unwrap();
        app.add_decoded_message(fixture_view("K2BBB", -5))
            .await
            .unwrap();
        // Newest-first: row 0 = K2BBB, row 1 = K1AAA.
        assert_eq!(
            app.displayed_messages()[1].call_sign.as_deref(),
            Some("K1AAA")
        );

        let rect = operate_panel_rect(&app, ActivePanel::BandActivity);
        // border(1) + header(1) + row 1 (K1AAA).
        let y = rect.y + 2 + 1;
        let x = rect.x + 2;

        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(cmd.is_none(), "click-to-focus never sends a TuiCommand");
        assert_eq!(app.active_panel, ActivePanel::BandActivity);
        assert_eq!(app.band_activity_scroll, 1);
        assert_eq!(
            app.band_activity_pinned_call.as_deref(),
            Some("K1AAA"),
            "click must pin via the SAME pin_band_activity_selection Up/Down uses"
        );
    }

    #[tokio::test]
    async fn click_dx_hunter_row_focuses_and_pins_it() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::Callers;
        app.dx_stations.insert(
            "W1AW".to_string(),
            dx_fixture("W1AW", 50, -10, 14_074_000.0, false),
        );

        let rect = operate_panel_rect(&app, ActivePanel::DxHunter);
        let y = rect.y + 2; // border(1) + header(1) + row 0
        let x = rect.x + 2;

        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(cmd.is_none());
        assert_eq!(app.active_panel, ActivePanel::DxHunter);
        assert_eq!(app.dx_hunter_pinned_call.as_deref(), Some("W1AW"));
    }

    #[tokio::test]
    async fn click_outside_every_panel_is_a_no_op() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::BandActivity;

        // Row 0 of the terminal is the title bar — outside the content
        // area `view_rects` was computed from, so no panel rect contains
        // it (mirrors the brief's "point on the status bar" case).
        let cmd = app.handle_mouse_event(left_click(5, 0)).await.unwrap();
        assert!(cmd.is_none());
        assert_eq!(
            app.active_panel,
            ActivePanel::BandActivity,
            "a miss must not change focus"
        );
    }

    #[tokio::test]
    async fn click_tx_placement_non_strip_row_only_focuses_no_park() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::Callers;
        app.apply_placement(placement_view_with_bin_code(0, 3));

        let rect = operate_panel_rect(&app, ActivePanel::TxPlacement);
        // border(1) + row 3 (park line) — NOT the strip row (row 0).
        let y = rect.y + 1 + 3;
        let x = rect.x + 2;

        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(cmd.is_none(), "non-strip row must not park");
        assert_eq!(app.active_panel, ActivePanel::TxPlacement);
        assert!(app.tx_offset_hold_hz.is_none(), "no park happened");
    }

    /// Clicking the openness strip on a fully-clear bin (code 3) parks
    /// normally via the EXISTING `SetTxOffset` command path.
    #[tokio::test]
    async fn click_tx_placement_strip_clear_bin_parks() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::Callers;
        // Every bin clear (code 3) — whatever bin the click lands on will
        // be code 3.
        app.apply_placement(placement_view_with_bin_code(0, 3));

        let rect = operate_panel_rect(&app, ActivePanel::TxPlacement);
        let y = rect.y + 1; // border(1) + row 0 (strip)
        let x = rect.x + 1 + 5; // a few columns into the strip

        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        match cmd {
            Some(crate::tui_runner::TuiCommand::SetTxOffset {
                offset_hz: Some(hz),
            }) => {
                assert_eq!(app.tx_offset_hold_hz, Some(hz));
                assert_eq!(app.tx_freq_mode, pancetta_core::TxFreqMode::Hold);
                assert!(app.parked_since.is_some());
                assert!(
                    app.status_message.contains(&format!("Parked at {hz} Hz")),
                    "status: {}",
                    app.status_message
                );
            }
            other => panic!("expected SetTxOffset(Some(_)), got {other:?}"),
        }
    }

    /// Collision-aware refusal (spec §2): a bin busy in BOTH windows
    /// (openness code 0) refuses to park — no `SetTxOffset` is sent, and
    /// `tx_offset_hold_hz` is left untouched.
    #[tokio::test]
    async fn click_tx_placement_strip_busy_both_windows_refuses_to_park() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::Callers;

        let rect = operate_panel_rect(&app, ActivePanel::TxPlacement);
        let y = rect.y + 1;
        let x = rect.x + 1 + 5;

        // Figure out which bin this column resolves to, then mark THAT bin
        // busy-both (code 0) so the click is guaranteed to land on it.
        let inner_width = (rect.width - 2) as usize;
        let strip_width = crate::ui::tx_placement::strip_width_for(inner_width);
        let col = (x - (rect.x + 1)) as usize;
        let range = (200.0, 2600.0);
        let freq = crate::ui::tx_placement::freq_for_col(col, strip_width, range).unwrap();
        let bin = crate::ui::tx_placement::bin_index_for_freq(freq, range, 25.0, 96).unwrap();
        app.apply_placement(placement_view_with_bin_code(bin, 0));

        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(cmd.is_none(), "busy-both-windows must refuse to park");
        assert!(
            app.tx_offset_hold_hz.is_none(),
            "no park should have been latched"
        );
        assert!(
            app.status_message
                .contains("busy in both windows — not parking"),
            "status: {}",
            app.status_message
        );
    }

    /// Codes 1/2 (busy in exactly one window) still park, but the status
    /// message names which window is busy.
    #[tokio::test]
    async fn click_tx_placement_strip_partial_busy_parks_and_names_window() {
        let mut app = fixture_app().await;
        app.terminal_size = (120, 40);
        app.active_panel = ActivePanel::Callers;

        let rect = operate_panel_rect(&app, ActivePanel::TxPlacement);
        let y = rect.y + 1;
        let x = rect.x + 1 + 5;
        let inner_width = (rect.width - 2) as usize;
        let strip_width = crate::ui::tx_placement::strip_width_for(inner_width);
        let col = (x - (rect.x + 1)) as usize;
        let range = (200.0, 2600.0);
        let freq = crate::ui::tx_placement::freq_for_col(col, strip_width, range).unwrap();
        let bin = crate::ui::tx_placement::bin_index_for_freq(freq, range, 25.0, 96).unwrap();

        // Code 1 = second (Odd) window clear, first (Even) window busy.
        app.apply_placement(placement_view_with_bin_code(bin, 1));
        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(matches!(
            cmd,
            Some(crate::tui_runner::TuiCommand::SetTxOffset { offset_hz: Some(_) })
        ));
        assert!(app.tx_offset_hold_hz.is_some(), "code 1 still parks");
        assert!(
            app.status_message.contains("Even window busy"),
            "status: {}",
            app.status_message
        );

        // Code 2 = first (Even) window clear, second (Odd) window busy.
        app.tx_offset_hold_hz = None;
        app.apply_placement(placement_view_with_bin_code(bin, 2));
        let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
        assert!(matches!(
            cmd,
            Some(crate::tui_runner::TuiCommand::SetTxOffset { offset_hz: Some(_) })
        ));
        assert!(app.tx_offset_hold_hz.is_some(), "code 2 still parks");
        assert!(
            app.status_message.contains("Odd window busy"),
            "status: {}",
            app.status_message
        );
    }
}
