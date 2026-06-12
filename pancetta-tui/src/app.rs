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
            let dx_station = DxStation {
                call_sign: call_sign.clone(),
                grid_square,
                frequency: message.frequency,
                mode: message.mode.clone(),
                last_seen: message.timestamp,
                snr: message.snr,
                distance: message.distance,
                bearing: message.bearing,
                worked_before: false, // TODO: Check logbook
                priority_score: self.calculate_dx_priority(&message),
                source: SpotSource::Local,
                rarity_tier: None,
                reporter_count: None,
                is_notable: false,
                notable_type: None,
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
            };

            self.dx_stations.insert(call_sign.clone(), dx_station);
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

    pub fn update_signal_strength(&mut self, strength: f32) {
        self.audio_level = strength;
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

    pub fn add_dx_spot(&mut self, callsign: String, freq: f64, mode: String, snr: i32) {
        let dx_station = DxStation {
            call_sign: callsign.clone(),
            grid_square: None,
            frequency: freq,
            mode,
            last_seen: Utc::now(),
            snr,
            distance: None,
            bearing: None,
            worked_before: false,
            priority_score: 0,
            source: SpotSource::Local,
            rarity_tier: None,
            reporter_count: None,
            is_notable: false,
            notable_type: None,
            confidence: None,
            best_snr_network: None,
            last_seen_network: None,
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
                let mut stations: Vec<&DxStation> = self.dx_stations.values().collect();
                stations.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                let station = stations.get(self.dx_hunter_scroll)?;
                if station.call_sign.is_empty() {
                    return None;
                }
                // DX Hunter spots only carry the dial frequency, not where in
                // the passband the station was. Default to the FT8 calling
                // convention (1500 Hz). Tuning is the operator's job after
                // the call kicks off. No slot parity is available from spots.
                Some((station.call_sign.clone(), 1500, None))
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
    pub fn merge_spot_groups(&mut self, spots: &[crate::tui_runner::CqdxSpotInfo]) {
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
    pub fn find_clear_offset(&self) -> Option<f64> {
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

        let mut best: Option<(f64, f64)> = None; // (offset, score)
        let mut hz = MIN_HZ;
        while hz <= MAX_HZ {
            let near_decode = recent_decodes_in_parity
                .iter()
                .any(|&f| (f - hz).abs() <= SEPARATION_HZ);
            let near_own = own_freqs.iter().any(|&f| (f - hz).abs() <= SEPARATION_HZ);

            if !near_decode && !near_own {
                let spectral = if let Some(row) = latest_row {
                    spectral_peak(row, hz, NEIGHBOR_HZ, SPECTRAL_RANGE_HZ)
                } else {
                    0.0
                };
                let center_bias = ((hz - 1500.0).abs() / 1300.0) * 0.3;
                let score = spectral as f64 + center_bias;
                best = match best {
                    Some((_, prev)) if prev <= score => best,
                    _ => Some((hz, score)),
                };
            }
            hz += STEP_HZ;
        }
        best.map(|(hz, _)| hz)
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

        let pick = app.find_clear_offset().expect("should find a clear spot");
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
    async fn find_clear_offset_returns_none_when_band_saturated() {
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

        let pick = app.find_clear_offset();
        assert!(
            pick.is_none(),
            "should refuse to pick when nothing is clear"
        );
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
}
