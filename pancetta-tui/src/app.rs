use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::{Config, Theme};

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
}

#[derive(Debug, Clone)]
pub struct QsoStatus {
    pub active: bool,
    pub call_sign: Option<String>,
    pub frequency: Option<f64>,
    pub mode: Option<String>,
    pub snr_tx: Option<i32>,
    pub snr_rx: Option<i32>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_tx: Option<DateTime<Utc>>,
    pub last_rx: Option<DateTime<Utc>>,
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

impl DeviceSelectionState {
    pub fn new() -> Self {
        Self {
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            selected_input_idx: 0,
            selected_output_idx: 0,
            active_panel: DevicePanel::Input,
            visible: false,
        }
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
    pub station_info: StationInfo,
    pub dx_stations: HashMap<String, DxStation>,
    pub band_activity_scroll: usize,
    pub dx_hunter_scroll: usize,

    // Audio processing
    pub audio_device: Option<String>,
    pub is_monitoring: bool,
    pub audio_level: f32,
    pub waterfall_data: Vec<Vec<f32>>,

    // Autonomous operator
    pub autonomous_status: Option<AutonomousStatus>,

    // Device selection modal
    pub device_selection: DeviceSelectionState,

    // Help overlay
    pub help_visible: bool,

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
            station_info,
            dx_stations: HashMap::new(),
            band_activity_scroll: 0,
            dx_hunter_scroll: 0,
            audio_device,
            is_monitoring: false,
            audio_level: 0.0,
            waterfall_data: Vec::new(),
            autonomous_status: None,
            device_selection: DeviceSelectionState::new(),
            help_visible: false,
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
        // When help is visible, consume Escape/F1/? to close it and swallow all other keys
        if self.help_visible {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => {
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
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
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

            // Theme switching
            KeyCode::Char('t') => {
                self.toggle_theme();
            }

            // Audio monitoring
            KeyCode::Char('m') => {
                self.toggle_monitoring().await?;
            }

            // Autonomous mode toggle
            KeyCode::Char('a') => {
                self.toggle_autonomous();
            }

            // Pause/resume autonomous
            KeyCode::Char('p') => {
                self.toggle_autonomous_pause();
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

    async fn toggle_monitoring(&mut self) -> Result<()> {
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

        // Limit message history
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
        static DEFAULT: std::sync::LazyLock<QsoStatus> = std::sync::LazyLock::new(|| QsoStatus {
            active: false,
            call_sign: None,
            frequency: None,
            mode: None,
            snr_tx: None,
            snr_rx: None,
            started_at: None,
            last_tx: None,
            last_rx: None,
            exchange_count: 0,
        });
        self.qso_statuses.first().unwrap_or(&DEFAULT)
    }

    /// Get a mutable reference to the primary QSO, creating one if needed.
    pub fn qso_status_mut(&mut self) -> &mut QsoStatus {
        if self.qso_statuses.is_empty() {
            self.qso_statuses.push(QsoStatus {
                active: false,
                call_sign: None,
                frequency: None,
                mode: None,
                snr_tx: None,
                snr_rx: None,
                started_at: None,
                last_tx: None,
                last_rx: None,
                exchange_count: 0,
            });
        }
        &mut self.qso_statuses[0]
    }

    pub fn update_qso_state(&mut self, active: bool, callsign: Option<String>) {
        let qso = self.qso_status_mut();
        qso.active = active;
        qso.call_sign = callsign;
        if active {
            qso.started_at = Some(Utc::now());
        }
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
            self.status_message = "Help — press Escape or F1 to close".to_string();
        } else {
            self.status_message = "Pancetta TUI Ready".to_string();
        }
    }

    /// Get the callsign and frequency of the currently selected station.
    ///
    /// Works from both Band Activity (decoded messages) and DX Hunter (spots).
    pub fn get_selected_station(&self) -> Option<(String, u64)> {
        match self.active_panel {
            ActivePanel::BandActivity => {
                // Display is reversed (newest first), so index from the end
                let msg = self
                    .decoded_messages
                    .iter()
                    .rev()
                    .nth(self.band_activity_scroll)?;
                let callsign = msg.call_sign.as_ref()?;
                if callsign.is_empty() {
                    return None;
                }
                let freq_hz = (msg.frequency * 1_000_000.0) as u64;
                Some((callsign.clone(), freq_hz))
            }
            ActivePanel::DxHunter => {
                let mut stations: Vec<&DxStation> = self.dx_stations.values().collect();
                stations.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                let station = stations.get(self.dx_hunter_scroll)?;
                if station.call_sign.is_empty() {
                    return None;
                }
                let freq_hz = (station.frequency * 1_000_000.0) as u64;
                Some((station.call_sign.clone(), freq_hz))
            }
            _ => None,
        }
    }

    pub fn activate_selected(&mut self) {
        if let Some((callsign, _freq)) = self.get_selected_station() {
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

    fn toggle_autonomous(&mut self) {
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
            self.status_message = "Autonomous mode not available".to_string();
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

    fn toggle_autonomous_pause(&mut self) {
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
}
