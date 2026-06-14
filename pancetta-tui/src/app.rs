use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::{Config, Theme};

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
    pub priority_score: u32,
    // CQDX network metadata
    pub source: SpotSource,
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
    StationInfo,
    DxHunter,
}

impl ActivePanel {
    pub fn next(&self) -> Self {
        match self {
            ActivePanel::BandActivity => ActivePanel::QsoStatus,
            ActivePanel::QsoStatus => ActivePanel::StationInfo,
            ActivePanel::StationInfo => ActivePanel::DxHunter,
            ActivePanel::DxHunter => ActivePanel::BandActivity,
        }
    }

    pub fn previous(&self) -> Self {
        match self {
            ActivePanel::BandActivity => ActivePanel::DxHunter,
            ActivePanel::QsoStatus => ActivePanel::BandActivity,
            ActivePanel::StationInfo => ActivePanel::QsoStatus,
            ActivePanel::DxHunter => ActivePanel::StationInfo,
        }
    }
}

pub struct App {
    pub config: Config,
    pub should_quit: bool,
    pub active_panel: ActivePanel,

    // UI State
    pub terminal_size: (u16, u16),
    pub status_message: String,
    pub theme: Theme,

    // Data
    pub decoded_messages: VecDeque<DecodedMessageView>,
    /// Active QSOs (supports multiple concurrent QSOs).
    pub qso_statuses: Vec<QsoStatus>,
    /// Snapshot of in-progress QSOs pushed by the coordinator on every
    /// QSO state change. Rendered in a 1-row banner above Band Activity
    /// so operators see who they're mid-conversation with at all times.
    pub active_qsos: Vec<ActiveQsoBanner>,
    pub station_info: StationInfo,
    pub dx_stations: HashMap<String, DxStation>,
    pub band_activity_scroll: usize,
    pub dx_hunter_scroll: usize,

    // Audio processing
    pub audio_device: Option<String>,
    pub is_monitoring: bool,
    pub audio_level: f32,
    /// Last rig S-meter reading, in hamlib STRENGTH convention: dB
    /// relative to S9 (0 = S9, -54 ≈ S0, +20 = S9+20). `None` until the
    /// first reading arrives (no rig, or rig doesn't report STRENGTH).
    pub signal_strength_db: Option<i32>,
    /// When `signal_strength_db` was last updated. Used to render the
    /// S-meter as stale ("---") if the rig stops reporting.
    pub signal_strength_at: Option<DateTime<Utc>>,
    pub pipeline_health: Option<PipelineHealth>,
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

    /// hb-161 — Phase 5 emergency-stop banner state. Flipped to `true`
    /// the moment the operator presses Shift+Q; the UI renders a red
    /// "STOPPED BY OPERATOR" banner over the main view. Cleared by Esc.
    /// The actual side effects (TX abort, autonomous-off, CQ-stop) are
    /// driven by the `TuiCommand::OperatorEmergencyStop` event the
    /// coordinator handles — this field is just the visual signal so
    /// the operator sees the keypress was received even if the
    /// command-forwarding loop is briefly stalled.
    pub stopped_by_operator: bool,

    // TX input
    pub tx_input_buffer: String,
    pub tx_input_cursor: usize,
    pub is_transmitting: bool,
    pub tx_frequency_offset: f64,

    // Band/frequency tracking
    pub current_band_index: usize,
    /// Frequency reported by the radio (via hamlib), if known. In MHz.
    pub radio_frequency: Option<f64>,

    // Communication channels
    pub message_rx: Option<mpsc::UnboundedReceiver<DecodedMessageView>>,
    pub audio_rx: Option<mpsc::UnboundedReceiver<Vec<f32>>>,
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
            mode: "FT8".to_string(),
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
            decoded_messages: VecDeque::with_capacity(1000),
            qso_statuses: Vec::new(),
            active_qsos: Vec::new(),
            station_info,
            dx_stations: HashMap::new(),
            band_activity_scroll: 0,
            dx_hunter_scroll: 0,
            audio_device,
            is_monitoring: false,
            audio_level: 0.0,
            signal_strength_db: None,
            signal_strength_at: None,
            pipeline_health: None,
            color_capability: ColorCapability::detect(),
            waterfall_data: Vec::new(),
            autonomous_status: None,
            device_selection: DeviceSelectionState::new(),
            help_visible: false,
            quit_confirm_visible: false,
            stopped_by_operator: false,
            tx_input_buffer: String::new(),
            tx_input_cursor: 0,
            is_transmitting: false,
            tx_frequency_offset: 1500.0,
            current_band_index: default_band_index,
            radio_frequency: None,
            message_rx: None,
            audio_rx: None,
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

    pub async fn tick(&mut self) -> Result<()> {
        // Collect audio data first
        let mut audio_batches = Vec::new();
        if let Some(ref mut audio_rx) = self.audio_rx {
            while let Ok(audio_data) = audio_rx.try_recv() {
                audio_batches.push(audio_data);
            }
        }

        // Process collected audio data
        for audio_data in audio_batches {
            self.process_audio_data(audio_data).await?;
        }

        // Collect decoded messages first
        let mut message_batch = Vec::new();
        if let Some(ref mut message_rx) = self.message_rx {
            while let Ok(message) = message_rx.try_recv() {
                message_batch.push(message);
            }
        }

        // Process collected messages
        for message in message_batch {
            self.add_decoded_message(message).await?;
        }

        // Clean up old messages
        self.cleanup_old_data();

        Ok(())
    }

    pub async fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // When help is visible, consume Escape/? to close it and swallow all other keys
        if self.help_visible {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.toggle_help();
                }
                _ => {} // swallow all other keys
            }
            return Ok(false);
        }

        match key.code {
            // Global shortcuts
            KeyCode::Esc => {
                self.should_quit = true;
                return Ok(true);
            }

            // Panel navigation
            KeyCode::Tab => {
                self.active_panel = self.active_panel.next();
                debug!("Switched to panel: {:?}", self.active_panel);
            }
            KeyCode::BackTab => {
                self.active_panel = self.active_panel.previous();
                debug!("Switched to panel: {:?}", self.active_panel);
            }

            // Panel-specific shortcuts
            KeyCode::Char('1') => self.active_panel = ActivePanel::BandActivity,
            KeyCode::Char('2') => self.active_panel = ActivePanel::QsoStatus,
            KeyCode::Char('3') => self.active_panel = ActivePanel::StationInfo,
            KeyCode::Char('4') => self.active_panel = ActivePanel::DxHunter,

            // Scrolling
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down();
            }
            KeyCode::PageUp => {
                for _ in 0..10 {
                    self.scroll_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..10 {
                    self.scroll_down();
                }
            }

            // Clear messages
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_messages();
            }

            _ => {}
        }

        Ok(false)
    }

    pub async fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<()> {
        use crossterm::event::MouseEventKind;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.scroll_down();
            }
            _ => {}
        }
        Ok(())
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

        // Update DX stations list
        if let Some(ref call_sign) = message.call_sign {
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
                            priority_score: local_score,
                            source: SpotSource::Local,
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

        self.status_message = format!("Decoded: {}", message.message);
        Ok(())
    }

    fn calculate_dx_priority(&self, message: &DecodedMessageView) -> u32 {
        let mut score = 0u32;

        // Higher SNR gets more points
        if message.snr > 0 {
            score += message.snr as u32;
        }

        // Distance bonus
        if let Some(distance) = message.distance {
            if distance > 1000.0 {
                score += 50;
            }
            if distance > 5000.0 {
                score += 100;
            }
        }

        // TODO: Add more sophisticated scoring based on:
        // - DXCC entity
        // - Band/mode combinations worked
        // - Contest status
        // - Propagation conditions

        score
    }

    fn scroll_up(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                if self.band_activity_scroll > 0 {
                    self.band_activity_scroll -= 1;
                }
            }
            ActivePanel::DxHunter => {
                if self.dx_hunter_scroll > 0 {
                    self.dx_hunter_scroll -= 1;
                }
            }
            _ => {}
        }
    }

    fn scroll_down(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                let max_scroll = self.decoded_messages.len().saturating_sub(1);
                if self.band_activity_scroll < max_scroll {
                    self.band_activity_scroll += 1;
                }
            }
            ActivePanel::DxHunter => {
                let max_scroll = self.dx_stations.len().saturating_sub(1);
                if self.dx_hunter_scroll < max_scroll {
                    self.dx_hunter_scroll += 1;
                }
            }
            _ => {}
        }
    }

    /// Number of rows a PageUp/PageDown moves the focused list.
    const SCROLL_PAGE: usize = 10;

    /// Jump the focused list to the top — for Band Activity that's the
    /// newest decode (realtime), since the list is newest-first. This is the
    /// "get me back to live" shortcut (Home / `g`), no more holding the arrow.
    pub fn scroll_to_top(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => self.band_activity_scroll = 0,
            ActivePanel::DxHunter => self.dx_hunter_scroll = 0,
            _ => {}
        }
    }

    /// Jump the focused list to the bottom — for Band Activity the oldest
    /// retained decode (End / `G`).
    pub fn scroll_to_bottom(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                self.band_activity_scroll = self.decoded_messages.len().saturating_sub(1);
            }
            ActivePanel::DxHunter => {
                self.dx_hunter_scroll = self.dx_stations.len().saturating_sub(1);
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
            }
            ActivePanel::DxHunter => {
                self.dx_hunter_scroll = self.dx_hunter_scroll.saturating_sub(Self::SCROLL_PAGE);
            }
            _ => {}
        }
    }

    /// Page toward the bottom (older entries).
    pub fn page_down(&mut self) {
        match self.active_panel {
            ActivePanel::BandActivity => {
                let max = self.decoded_messages.len().saturating_sub(1);
                self.band_activity_scroll =
                    (self.band_activity_scroll + Self::SCROLL_PAGE).min(max);
            }
            ActivePanel::DxHunter => {
                let max = self.dx_stations.len().saturating_sub(1);
                self.dx_hunter_scroll = (self.dx_hunter_scroll + Self::SCROLL_PAGE).min(max);
            }
            _ => {}
        }
    }

    fn toggle_theme(&mut self) {
        self.theme = match self.theme {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        };
        self.status_message = format!("Switched to {:?} theme", self.theme);
        info!("Theme switched to: {:?}", self.theme);
    }

    pub fn clear_messages(&mut self) {
        self.decoded_messages.clear();
        self.band_activity_scroll = 0;
        self.status_message = "Messages cleared".to_string();
        info!("Cleared all decoded messages");
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

    /// Switch to the next band (higher frequency). Returns the new FT8 dial frequency in Hz.
    pub fn band_up(&mut self) -> u64 {
        let num_bands = self.config.bands.bands.len();
        if num_bands == 0 {
            return (self.station_info.operating_frequency * 1_000_000.0) as u64;
        }
        self.current_band_index = (self.current_band_index + 1) % num_bands;
        self.apply_band_selection()
    }

    /// Switch to the previous band (lower frequency). Returns the new FT8 dial frequency in Hz.
    pub fn band_down(&mut self) -> u64 {
        let num_bands = self.config.bands.bands.len();
        if num_bands == 0 {
            return (self.station_info.operating_frequency * 1_000_000.0) as u64;
        }
        self.current_band_index = (self.current_band_index + num_bands - 1) % num_bands;
        self.apply_band_selection()
    }

    /// Apply the current band selection, updating operating frequency.
    /// Returns the FT8 dial frequency in Hz.
    fn apply_band_selection(&mut self) -> u64 {
        let band = &self.config.bands.bands[self.current_band_index];
        self.station_info.operating_frequency = band.ft8_frequency;
        self.status_message = format!("Band: {} — {:.3} MHz", band.name, band.ft8_frequency);
        // Clear band activity when switching bands
        self.decoded_messages.clear();
        self.band_activity_scroll = 0;
        (band.ft8_frequency * 1_000_000.0) as u64
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

    /// S-meter display, or `None` when no reading exists or the last
    /// reading is stale (rig stopped reporting > 10s ago). Formatting
    /// follows the hamlib STRENGTH convention (0 dB = S9, 6 dB per
    /// S-unit below S9).
    pub fn s_meter_display(&self) -> Option<String> {
        const STALE_AFTER_SECS: i64 = 10;
        let db = self.signal_strength_db?;
        let at = self.signal_strength_at?;
        if (Utc::now() - at).num_seconds() > STALE_AFTER_SECS {
            return None;
        }
        Some(format_s_meter(db))
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
    pub fn apply_active_qsos(&mut self, qsos: Vec<ActiveQsoBanner>) {
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
            })
            .collect();
        self.active_qsos = qsos;
    }

    pub fn add_dx_spot(
        &mut self,
        callsign: String,
        freq: f64,
        mode: String,
        snr: i32,
        worked_before: bool,
    ) {
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
            priority_score: 0,
            source: SpotSource::Local,
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
    }

    pub fn add_error_message(&mut self, error: String) {
        self.status_message = format!("Error: {}", error);
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
        let others: Vec<&DecodedMessageView> = self
            .decoded_messages
            .iter()
            .rev()
            .filter(|m| !m.is_directed_at_us)
            .collect();
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
        let mut list: Vec<&DxStation> = self.dx_stations.values().collect();
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
                    priority_score: 0,
                    source: SpotSource::Network,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    fn fixture_view_directed(call: &str, snr: i32) -> DecodedMessageView {
        let mut v = fixture_view(call, snr);
        v.is_directed_at_us = true;
        v.message = format!("K5ARH {} -10", call);
        v
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
        }
    }

    /// Batch 94: an active-QSOs snapshot populates the QSO-detail panel
    /// entries (qso_statuses) alongside the banner list.
    #[tokio::test]
    async fn apply_active_qsos_populates_detail_panel() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        assert!(app.qso_statuses.is_empty());

        app.apply_active_qsos(vec![fixture_banner("JA1ABC", "wait rpt", None)]);

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

    /// Stale QSOs leave the detail panel when the next snapshot omits
    /// them — and an empty snapshot returns the panel to STANDBY.
    #[tokio::test]
    async fn apply_active_qsos_empty_snapshot_clears_panel() {
        let mut app = App::new(Config::default(), None).await.unwrap();
        app.apply_active_qsos(vec![fixture_banner("JA1ABC", "wait rpt", None)]);
        assert_eq!(app.qso_statuses.len(), 1);

        app.apply_active_qsos(Vec::new());
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
        app.apply_active_qsos(vec![banner]);
        assert_eq!(app.qso_statuses[0].snr_rx, Some(-8));
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
        for i in 47..54 {
            row[i] = 1.0;
        }
        app.waterfall_data.push(row);

        let (pick, is_clear) = app.find_clear_offset().expect("should find a clear spot");
        assert!(
            is_clear,
            "mostly-empty band should yield a truly clear slot"
        );
        // Should land outside 1400-1600 ± 75 Hz separation.
        assert!(
            pick < 1325.0 || pick > 1675.0,
            "picked {} which is too close to busy band",
            pick
        );
        // Should be in the allowed range (200..2800).
        assert!(pick >= 200.0 && pick <= 2800.0);
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
        assert!(pick >= 200.0 && pick <= 2800.0);
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
            priority_score: priority,
            source: SpotSource::Local,
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
}
