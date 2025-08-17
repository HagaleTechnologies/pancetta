// TUI Runner - Main loop for terminal user interface
//
// This module implements the main TUI event loop with message bus integration,
// real-time updates, and efficient rendering.

use anyhow::Result;
use chrono::Timelike;
use crossbeam_channel::{Receiver, Sender};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::app::{App, DecodedMessage};
use crate::config::Config;

/// TUI runner that manages the terminal interface
pub struct TuiRunner {
    /// Application state
    app: Arc<RwLock<App>>,
    /// TUI configuration
    config: Config,
    /// Terminal instance
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Message receiver from message bus
    message_rx: Receiver<TuiMessage>,
    /// Message sender to message bus
    message_tx: Sender<TuiCommand>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Frame rate limiter
    last_render: Instant,
    /// Target FPS
    target_fps: u32,
    /// Performance metrics
    metrics: TuiMetrics,
}

/// Messages received by the TUI
#[derive(Debug, Clone)]
pub enum TuiMessage {
    /// Decoded FT8 message
    DecodedMessage(DecodedMessage),
    /// Frequency update
    FrequencyUpdate { vfo: u8, frequency: u64 },
    /// Signal strength update
    SignalStrengthUpdate { dbm: i32 },
    /// QSO state update
    QsoStateUpdate { qso_id: String, state: String },
    /// DX spot
    DxSpot { callsign: String, frequency: u64, spotter: String },
    /// Error message
    Error { component: String, message: String },
    /// Status update
    StatusUpdate { component: String, status: String },
}

/// Commands sent from TUI
#[derive(Debug, Clone)]
pub enum TuiCommand {
    /// Change frequency
    SetFrequency { vfo: u8, frequency: u64 },
    /// Start CQ
    StartCq,
    /// Stop CQ
    StopCq,
    /// Send message
    SendMessage { text: String },
    /// Toggle PTT
    TogglePtt,
    /// Clear decoded messages
    ClearMessages,
    /// Request status
    RequestStatus,
}

/// TUI performance metrics
#[derive(Debug, Default, Clone)]
struct TuiMetrics {
    frames_rendered: u64,
    messages_processed: u64,
    avg_render_time_ms: f64,
    peak_render_time_ms: f64,
    dropped_frames: u64,
}

impl TuiRunner {
    /// Create new TUI runner
    pub fn new(
        app: Arc<RwLock<App>>,
        config: Config,
        message_rx: Receiver<TuiMessage>,
        message_tx: Sender<TuiCommand>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        
        Ok(Self {
            app,
            config,
            terminal,
            message_rx,
            message_tx,
            shutdown,
            last_render: Instant::now(),
            target_fps: 30,
            metrics: TuiMetrics::default(),
        })
    }
    
    /// Run the TUI main loop
    pub async fn run(mut self) -> Result<()> {
        info!("Starting TUI main loop");
        
        let frame_duration = Duration::from_millis(1000 / self.target_fps as u64);
        let mut event_timeout = Duration::from_millis(50);
        
        loop {
            // Check shutdown signal
            if self.shutdown.load(Ordering::Relaxed) {
                info!("TUI shutdown requested");
                break;
            }
            
            // Process incoming messages (non-blocking)
            self.process_messages().await?;
            
            // Handle user input (with timeout)
            if event::poll(event_timeout)? {
                if let Event::Key(key) = event::read()? {
                    if !self.handle_key_event(key).await? {
                        break; // User requested quit
                    }
                }
            }
            
            // Render frame if needed (rate limited)
            if self.last_render.elapsed() >= frame_duration {
                let render_start = Instant::now();
                
                self.render_frame().await?;
                
                let render_time = render_start.elapsed();
                self.update_metrics(render_time);
                
                self.last_render = Instant::now();
            } else {
                // Small yield to prevent busy waiting
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            
            // Adaptive timeout based on activity
            event_timeout = if self.metrics.messages_processed > 0 {
                Duration::from_millis(10) // More responsive when active
            } else {
                Duration::from_millis(50) // Less CPU when idle
            };
        }
        
        self.cleanup()?;
        info!("TUI main loop completed");
        Ok(())
    }
    
    /// Process incoming messages from message bus
    async fn process_messages(&mut self) -> Result<()> {
        let mut message_count = 0;
        const MAX_MESSAGES_PER_FRAME: usize = 10;
        
        // Process up to MAX_MESSAGES_PER_FRAME to avoid blocking render
        while message_count < MAX_MESSAGES_PER_FRAME {
            match self.message_rx.try_recv() {
                Ok(message) => {
                    self.handle_message(message).await?;
                    message_count += 1;
                    self.metrics.messages_processed += 1;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    warn!("TUI message channel disconnected");
                    return Ok(());
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle incoming message
    async fn handle_message(&mut self, message: TuiMessage) -> Result<()> {
        let mut app = self.app.write().await;
        
        match message {
            TuiMessage::DecodedMessage(decoded) => {
                app.add_decoded_message(decoded);
            }
            TuiMessage::FrequencyUpdate { vfo: _, frequency } => {
                app.update_frequency(frequency);
            }
            TuiMessage::SignalStrengthUpdate { dbm } => {
                app.update_signal_strength(dbm as f32);
            }
            TuiMessage::QsoStateUpdate { qso_id, state: _ } => {
                // Parse QSO state - for now just check if active
                let active = !qso_id.is_empty();
                let callsign = if active { Some(qso_id) } else { None };
                app.update_qso_state(active, callsign);
            }
            TuiMessage::DxSpot { callsign, frequency, spotter: _ } => {
                // For now use FT8 as default mode
                app.add_dx_spot(callsign, frequency as f64, "FT8".to_string(), 0);
            }
            TuiMessage::Error { component: _, message } => {
                app.add_error_message(message);
            }
            TuiMessage::StatusUpdate { component, status } => {
                app.update_component_status(component, status);
            }
        }
        
        Ok(())
    }
    
    /// Handle keyboard events
    async fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        let mut app = self.app.write().await;
        
        match key.code {
            // Quit
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                return Ok(false);
            }
            
            // Panel navigation
            KeyCode::Tab => {
                app.next_panel();
            }
            KeyCode::BackTab => {
                app.previous_panel();
            }
            
            // Arrow keys for list navigation
            KeyCode::Up => {
                app.previous_item();
            }
            KeyCode::Down => {
                app.next_item();
            }
            KeyCode::Left => {
                app.previous_page();
            }
            KeyCode::Right => {
                app.next_page();
            }
            
            // Function keys
            KeyCode::F(1) => {
                // F1 - Help
                app.toggle_help();
            }
            KeyCode::F(2) => {
                // F2 - Start CQ
                self.message_tx.send(TuiCommand::StartCq)?;
            }
            KeyCode::F(3) => {
                // F3 - Stop CQ
                self.message_tx.send(TuiCommand::StopCq)?;
            }
            KeyCode::F(5) => {
                // F5 - Clear messages
                app.clear_messages();
                self.message_tx.send(TuiCommand::ClearMessages)?;
            }
            KeyCode::F(9) => {
                // F9 - Toggle PTT
                self.message_tx.send(TuiCommand::TogglePtt)?;
            }
            
            // Space - Select/activate
            KeyCode::Char(' ') => {
                app.activate_selected();
            }
            
            // Enter - Send message or confirm
            KeyCode::Enter => {
                let text = app.get_input_text();
                if !text.is_empty() {
                    self.message_tx.send(TuiCommand::SendMessage { text })?;
                    app.clear_input();
                }
            }
            
            // Text input
            KeyCode::Char(c) => {
                app.input_char(c);
            }
            KeyCode::Backspace => {
                app.delete_char();
            }
            
            _ => {}
        }
        
        Ok(true)
    }
    
    /// Render a frame
    async fn render_frame(&mut self) -> Result<()> {
        let app = self.app.read().await;
        let metrics = self.metrics.clone();
        let last_render = self.last_render;
        
        self.terminal.draw(|f| {
            let size = f.area();
            
            // Main layout
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),    // Header
                    Constraint::Min(10),      // Main content
                    Constraint::Length(3),    // Status bar
                ])
                .split(size);
            
            // Render header inline
            let header_text = format!(
                " Pancetta FT8 | {} | {} MHz | {} ",
                app.station_info.call_sign,
                app.station_info.operating_frequency / 1_000_000.0,
                app.station_info.mode
            );
            let header = Paragraph::new(header_text)
                .style(Style::default().bg(Color::Blue).fg(Color::White));
            f.render_widget(header, chunks[0]);
            
            // Render main content inline
            TuiRunner::render_main_content_static(f, chunks[1], &app);
            
            // Render status bar inline
            let status_text = format!(
                " TX: {} | S-meter: {} | FPS: {} | F1:Help F2:CQ F5:Clear Q:Quit ",
                if app.is_monitoring { "ON" } else { "OFF" },
                app.audio_level as i32,
                metrics.frames_rendered / last_render.elapsed().as_secs().max(1)
            );
            let status = Paragraph::new(status_text)
                .style(Style::default().bg(Color::Gray).fg(Color::White));
            f.render_widget(status, chunks[2]);
        })?;
        
        self.metrics.frames_rendered += 1;
        Ok(())
    }
    
    /// Static version of render_main_content for use in closure
    fn render_main_content_static(f: &mut Frame, area: Rect, app: &App) {
        // Split into panels
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(60),  // Band activity
                Constraint::Percentage(40),  // Right panels
            ])
            .split(area);
        
        // Render band activity on the left
        Self::render_band_activity_static(f, chunks[0], app);
        
        // Split right side into DX and QSO panels
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),  // DX stations
                Constraint::Percentage(50),  // QSO status
            ])
            .split(chunks[1]);
        
        Self::render_dx_stations_static(f, right_chunks[0], app);
        Self::render_qso_status_static(f, right_chunks[1], app);
    }
    
    /// Static version of render_band_activity for use in closure
    fn render_band_activity_static(f: &mut Frame, area: Rect, app: &App) {
        let messages: Vec<ListItem> = app
            .decoded_messages
            .iter()
            .map(|msg| {
                let style = if msg.message.contains("CQ") {
                    Style::default().fg(Color::Yellow)
                } else if msg.call_sign.as_ref().map_or(false, |c| c == &app.station_info.call_sign) {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                
                let text = format!(
                    "{:02}:{:02}:{:02} {:>4.0} {:>3} {}",
                    msg.timestamp.naive_local().hour(),
                    msg.timestamp.naive_local().minute(),
                    msg.timestamp.naive_local().second(),
                    msg.delta_freq,
                    msg.snr,
                    msg.message
                );
                
                ListItem::new(text).style(style)
            })
            .collect();
        
        let messages_list = List::new(messages)
            .block(
                Block::default()
                    .title(" Band Activity ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        
        f.render_widget(messages_list, area);
    }
    
    /// Static version of render_dx_stations for use in closure
    fn render_dx_stations_static(f: &mut Frame, area: Rect, app: &App) {
        let dx_items: Vec<ListItem> = app
            .dx_stations
            .iter()
            .map(|dx| {
                ListItem::new(format!(
                    "{} {} {:>6.0}km {}dB",
                    dx.1.call_sign, 
                    dx.1.grid_square.as_ref().unwrap_or(&"----".to_string()),
                    dx.1.distance.unwrap_or(0.0),
                    dx.1.snr
                ))
            })
            .collect();
        
        let dx_list = List::new(dx_items)
            .block(
                Block::default()
                    .title(" DX Stations ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            );
        
        f.render_widget(dx_list, area);
    }
    
    /// Static version of render_qso_status for use in closure
    fn render_qso_status_static(f: &mut Frame, area: Rect, app: &App) {
        let qso_text = if app.qso_status.active {
            format!(
                "QSO with: {}\nTX: {} dB\nRX: {} dB\nExchanges: {}",
                app.qso_status.call_sign.as_ref().unwrap_or(&"Unknown".to_string()),
                app.qso_status.snr_tx.unwrap_or(0),
                app.qso_status.snr_rx.unwrap_or(0),
                app.qso_status.exchange_count
            )
        } else {
            "No active QSO".to_string()
        };
        
        let qso_status = Paragraph::new(qso_text)
            .block(
                Block::default()
                    .title(" QSO Status ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: true });
        
        f.render_widget(qso_status, area);
    }
    
    /// Render header
    fn render_header(&self, f: &mut Frame, area: Rect, app: &App) {
        let header_text = format!(
            " Pancetta FT8 | {} | {} MHz | {} ",
            app.station_info.call_sign,
            app.station_info.operating_frequency / 1_000_000.0,
            app.station_info.mode
        );
        
        let header = Paragraph::new(header_text)
            .style(Style::default().bg(Color::Blue).fg(Color::White))
            .block(Block::default().borders(Borders::NONE));
        
        f.render_widget(header, area);
    }
    
    /// Render main content area
    fn render_main_content(&self, f: &mut Frame, area: Rect, app: &App) {
        // Split into columns
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(60),  // Decoded messages
                Constraint::Percentage(40),  // Side panels
            ])
            .split(area);
        
        // Render decoded messages
        self.render_decoded_messages(f, chunks[0], app);
        
        // Render side panels
        self.render_side_panels(f, chunks[1], app);
    }
    
    /// Render decoded messages panel
    fn render_decoded_messages(&self, f: &mut Frame, area: Rect, app: &App) {
        let messages: Vec<ListItem> = app
            .decoded_messages
            .iter()
            .map(|msg| {
                let style = if msg.message.contains("CQ") {
                    Style::default().fg(Color::Yellow)
                } else if msg.call_sign.as_ref().map_or(false, |c| c == &app.station_info.call_sign) {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                
                let text = format!(
                    "{:02}:{:02}:{:02} {:>4.0} {:>3} {}",
                    msg.timestamp.naive_local().hour(),
                    msg.timestamp.naive_local().minute(),
                    msg.timestamp.naive_local().second(),
                    msg.delta_freq,
                    msg.snr,
                    msg.message
                );
                
                ListItem::new(text).style(style)
            })
            .collect();
        
        let messages_list = List::new(messages)
            .block(
                Block::default()
                    .title(" Decoded Messages ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        
        f.render_widget(messages_list, area);
    }
    
    /// Render side panels
    fn render_side_panels(&self, f: &mut Frame, area: Rect, app: &App) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),  // DX stations
                Constraint::Percentage(50),  // QSO status
            ])
            .split(area);
        
        // DX Stations
        let dx_items: Vec<ListItem> = app
            .dx_stations
            .iter()
            .map(|dx| {
                ListItem::new(format!(
                    "{} {} {:>6.0}km {}dB",
                    dx.1.call_sign, 
                    dx.1.grid_square.as_ref().unwrap_or(&"----".to_string()),
                    dx.1.distance.unwrap_or(0.0),
                    dx.1.snr
                ))
            })
            .collect();
        
        let dx_list = List::new(dx_items)
            .block(
                Block::default()
                    .title(" DX Stations ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            );
        
        f.render_widget(dx_list, chunks[0]);
        
        // QSO Status
        let qso_text = if app.qso_status.active {
            format!(
                "QSO with: {}\nTX: {} dB\nRX: {} dB\nExchanges: {}",
                app.qso_status.call_sign.as_ref().unwrap_or(&"Unknown".to_string()),
                app.qso_status.snr_tx.unwrap_or(0),
                app.qso_status.snr_rx.unwrap_or(0),
                app.qso_status.exchange_count
            )
        } else {
            "No active QSO".to_string()
        };
        
        let qso_status = Paragraph::new(qso_text)
            .block(
                Block::default()
                    .title(" QSO Status ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: true });
        
        f.render_widget(qso_status, chunks[1]);
    }
    
    /// Render status bar
    fn render_status_bar(&self, f: &mut Frame, area: Rect, app: &App) {
        let status_text = format!(
            " TX: {} | S-meter: {} | FPS: {} | F1:Help F2:CQ F5:Clear Q:Quit ",
            if app.is_monitoring { "ON" } else { "OFF" },
            app.audio_level as i32,
            self.metrics.frames_rendered / self.last_render.elapsed().as_secs().max(1)
        );
        
        let status = Paragraph::new(status_text)
            .style(Style::default().bg(Color::DarkGray).fg(Color::White))
            .block(Block::default().borders(Borders::NONE));
        
        f.render_widget(status, area);
    }
    
    /// Update performance metrics
    fn update_metrics(&mut self, render_time: Duration) {
        let render_ms = render_time.as_millis() as f64;
        
        // Update average (simple moving average)
        self.metrics.avg_render_time_ms = 
            (self.metrics.avg_render_time_ms * 0.9) + (render_ms * 0.1);
        
        // Update peak
        if render_ms > self.metrics.peak_render_time_ms {
            self.metrics.peak_render_time_ms = render_ms;
        }
        
        // Check for dropped frames
        let target_frame_time = 1000.0 / self.target_fps as f64;
        if render_ms > target_frame_time {
            self.metrics.dropped_frames += 1;
            debug!("Dropped frame: render took {:.2}ms", render_ms);
        }
    }
    
    /// Cleanup terminal on exit
    fn cleanup(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;
        
        info!(
            "TUI metrics - Frames: {}, Messages: {}, Avg render: {:.2}ms, Dropped: {}",
            self.metrics.frames_rendered,
            self.metrics.messages_processed,
            self.metrics.avg_render_time_ms,
            self.metrics.dropped_frames
        );
        
        Ok(())
    }
}

/// Create and run TUI with message bus integration
pub async fn run_tui_with_message_bus(
    config: Config,
    message_rx: Receiver<TuiMessage>,
    message_tx: Sender<TuiCommand>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    // Create app state
    let app = Arc::new(RwLock::new(
        App::new(config.clone(), None).await?
    ));
    
    // Create and run TUI runner
    let runner = TuiRunner::new(app, config, message_rx, message_tx, shutdown)?;
    runner.run().await
}