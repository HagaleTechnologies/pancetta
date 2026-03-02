use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::{Config, Theme};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedMessage {
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
    pub decoded_messages: VecDeque<DecodedMessage>,
    pub qso_status: QsoStatus,
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

    // Communication channels
    pub message_rx: Option<mpsc::UnboundedReceiver<DecodedMessage>>,
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

        let qso_status = QsoStatus {
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
        };

        let mut app = Self {
            config: config.clone(),
            should_quit: false,
            active_panel: ActivePanel::BandActivity,
            terminal_size: (80, 24),
            status_message: "Pancetta TUI Ready".to_string(),
            theme: config.ui.theme,
            decoded_messages: VecDeque::with_capacity(1000),
            qso_status,
            station_info,
            dx_stations: HashMap::new(),
            band_activity_scroll: 0,
            dx_hunter_scroll: 0,
            audio_device,
            is_monitoring: false,
            audio_level: 0.0,
            waterfall_data: Vec::new(),
            autonomous_status: None,
            message_rx: None,
            audio_rx: None,
        };

        // Initialize audio monitoring if device specified
        if let Some(device) = app.audio_device.clone() {
            app.start_audio_monitoring(&device).await?;
        }

        info!("App initialized with station {}", app.station_info.call_sign);
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
        match key.code {
            // Global shortcuts
            KeyCode::Char('q') | KeyCode::Esc => {
                if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Esc {
                    self.should_quit = true;
                    return Ok(true);
                }
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

    pub async fn handle_mouse_event(&mut self, _mouse: MouseEvent) -> Result<()> {
        // TODO: Implement mouse handling for scrolling and panel selection
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

    pub async fn handle_decoded_message(&mut self, message: DecodedMessage) -> Result<()> {
        self.add_decoded_message(message).await
    }

    async fn start_audio_monitoring(&mut self, _device: &str) -> Result<()> {
        // TODO: Initialize audio processing pipeline
        self.is_monitoring = true;
        self.status_message = "Audio monitoring started".to_string();
        info!("Started audio monitoring");
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
        
        info!("Audio monitoring: {}", if self.is_monitoring { "enabled" } else { "disabled" });
        Ok(())
    }

    async fn process_audio_data(&mut self, data: Vec<f32>) -> Result<()> {
        // Calculate audio level (RMS)
        let sum_squares: f32 = data.iter().map(|&x| x * x).sum();
        self.audio_level = (sum_squares / data.len() as f32).sqrt();

        // TODO: Add to waterfall display
        // For now, just store last N samples for waterfall
        if self.waterfall_data.len() > 100 {
            self.waterfall_data.remove(0);
        }
        
        // Simple frequency domain representation (placeholder)
        let fft_data: Vec<f32> = (0..64).map(|i| {
            (self.audio_level * (i as f32 / 64.0).sin()).abs()
        }).collect();
        
        self.waterfall_data.push(fft_data);
        Ok(())
    }

    pub async fn add_decoded_message(&mut self, message: DecodedMessage) -> Result<()> {
        debug!("Adding decoded message: {}", message.message);
        
        // Add to band activity
        self.decoded_messages.push_back(message.clone());
        
        // Limit message history
        while self.decoded_messages.len() > 1000 {
            self.decoded_messages.pop_front();
        }

        // Update DX stations list
        if let Some(ref call_sign) = message.call_sign {
            let dx_station = DxStation {
                call_sign: call_sign.clone(),
                grid_square: message.grid_square.clone(),
                frequency: message.frequency,
                mode: message.mode.clone(),
                last_seen: message.timestamp,
                snr: message.snr,
                distance: message.distance,
                bearing: message.bearing,
                worked_before: false, // TODO: Check logbook
                priority_score: self.calculate_dx_priority(&message),
            };
            
            self.dx_stations.insert(call_sign.clone(), dx_station);
        }

        self.status_message = format!("Decoded: {}", message.message);
        Ok(())
    }

    fn calculate_dx_priority(&self, message: &DecodedMessage) -> u32 {
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
        self.dx_stations.retain(|_, station| station.last_seen > cutoff);
    }

    // Missing public methods for tui_runner
    pub fn update_frequency(&mut self, freq: u64) {
        self.station_info.operating_frequency = freq as f64;
        self.status_message = format!("Frequency: {} Hz", freq);
    }

    pub fn update_signal_strength(&mut self, strength: f32) {
        self.audio_level = strength;
    }

    pub fn update_qso_state(&mut self, active: bool, callsign: Option<String>) {
        self.qso_status.active = active;
        self.qso_status.call_sign = callsign;
        if active {
            self.qso_status.started_at = Some(Utc::now());
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
        };
        self.dx_stations.insert(callsign, dx_station);
    }

    pub fn add_error_message(&mut self, error: String) {
        self.status_message = format!("Error: {}", error);
    }

    pub fn update_component_status(&mut self, component: String, status: String) {
        self.status_message = format!("{}: {}", component, status);
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
        // TODO: Implement help panel toggle
        self.status_message = "Help not yet implemented".to_string();
    }

    pub fn activate_selected(&mut self) {
        // TODO: Implement item activation
        self.status_message = "Activation not yet implemented".to_string();
    }

    pub fn get_input_text(&self) -> String {
        // TODO: Implement input text buffer
        String::new()
    }

    pub fn clear_input(&mut self) {
        // TODO: Clear input buffer
    }

    pub fn input_char(&mut self, c: char) {
        // TODO: Add character to input buffer
        self.status_message = format!("Input: {}", c);
    }

    pub fn delete_char(&mut self) {
        // TODO: Delete character from input buffer
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
            info!("Autonomous mode: {}", if status.enabled { "enabled" } else { "disabled" });
        } else {
            self.status_message = "Autonomous mode not available".to_string();
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