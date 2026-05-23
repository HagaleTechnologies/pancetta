// TUI Runner - Main loop for terminal user interface
//
// This module implements the main TUI event loop with message bus integration,
// real-time updates, and efficient rendering.

use anyhow::Result;
use chrono::Timelike;
use crossbeam_channel::{Receiver, Sender};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    },
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

use crate::app::{App, DecodedMessageView, DevicePanel, DeviceSelectionState};
use crate::config::Config;

/// TUI runner that manages the terminal interface
pub struct TuiRunner {
    /// Application state
    app: Arc<RwLock<App>>,
    /// TUI configuration
    config: Config,
    /// Terminal instance. `None` only in unit tests built via
    /// `new_for_test`, where constructing a `CrosstermBackend(stdout)`
    /// on CI hits EAGAIN on the terminal-size ioctl. The render and
    /// cleanup paths that touch `terminal` aren't exercised by the
    /// key-event unit tests, so `Option` is the smallest seam.
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
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

/// Lightweight spot info for TUI display (avoids pancetta-cqdx dependency)
#[derive(Debug, Clone)]
pub struct CqdxSpotInfo {
    pub dx_call: String,
    pub band: String,
    pub mode: String,
    pub frequency_hz: u64,
    pub grid: Option<String>,
    pub rarity_tier: String,
    pub reporter_count: u32,
    pub best_snr: Option<i32>,
    pub confidence: f64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub is_notable: bool,
    pub notable_type: Option<String>,
    pub entity_name: String,
}

/// Messages received by the TUI
#[derive(Debug, Clone)]
pub enum TuiMessage {
    /// Decoded FT8 message
    DecodedMessage(DecodedMessageView),
    /// Frequency update
    FrequencyUpdate { vfo: u8, frequency: u64 },
    /// Signal strength update
    SignalStrengthUpdate { dbm: i32 },
    /// QSO state update
    QsoStateUpdate { qso_id: String, state: String },
    /// DX spot
    DxSpot {
        callsign: String,
        frequency: u64,
        spotter: String,
    },
    /// Error message
    Error { component: String, message: String },
    /// Status update
    StatusUpdate { component: String, status: String },
    /// Waterfall display data (normalized power rows, each Vec<f32> is one time-slice)
    WaterfallUpdate { rows: Vec<Vec<f32>> },
    /// Live spot groups from cqdx.io
    SpotGroupUpdate { spots: Vec<CqdxSpotInfo> },
    /// Audio level update (RMS, 0.0-1.0)
    AudioLevel { level: f32 },
    /// Pipeline health snapshot (sent periodically by coordinator)
    PipelineHealth(crate::app::PipelineHealth),
    /// Snapshot of QSOs currently in progress, pushed by the QSO
    /// coordinator on every state change. The TUI replaces its
    /// previous active-QSOs list with this snapshot — sender
    /// owns the truth, receiver is a passive renderer.
    ActiveQsosUpdate {
        qsos: Vec<crate::app::ActiveQsoBanner>,
    },
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
    /// Abort the in-flight TX without exiting pancetta. Operator-pressed `h`.
    /// Distinct from `q` (whole-app shutdown, with confirm) and `s`/StopCq
    /// (turn off repeating CQ). Drops PTT within ~150ms via the same
    /// PttGuard mechanism the shutdown path uses.
    StopTx,
    /// Operator pressed `T` — find a clear TX audio offset and jump the
    /// cursor there. TUI-local: the handler calls `App::find_clear_offset`
    /// and updates `tx_frequency_offset` directly. No bus message needed.
    #[allow(dead_code)]
    // Part of the TuiCommand API for future remote-control or scripting use
    FindClearOffset,
    /// Toggle a single-tone tune transmission for antenna tuning. Maps
    /// to Shift+T. First press starts a 12-second tone; subsequent press
    /// while a tune is active aborts it. `h` (halt TX) also aborts.
    /// Coordinator owns the tone-active state; the TUI just sends the
    /// toggle.
    ToggleTune,
    /// Call a station (click-to-call from band activity)
    CallStation {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    },
    /// Clear decoded messages
    ClearMessages,
    /// Request status
    RequestStatus,
    /// Select audio devices by name
    SelectDevice {
        input_device: Option<String>,
        output_device: Option<String>,
    },
    /// User requested quit
    Quit,
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
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self {
            app,
            config,
            terminal: Some(terminal),
            message_rx,
            message_tx,
            shutdown,
            last_render: Instant::now(),
            target_fps: 30,
            metrics: TuiMetrics::default(),
        })
    }

    /// Create a TUI runner for unit tests — bypasses `enable_raw_mode`
    /// AND skips terminal construction entirely. `Terminal::new`
    /// internally queries the backend for its size, which on a
    /// `CrosstermBackend(io::stdout())` does a tty ioctl. In a
    /// headless CI runner that ioctl returns `EAGAIN`/
    /// `Resource temporarily unavailable` and the constructor panics.
    /// The key-event unit tests never render, so `terminal = None` is
    /// fine — render-path callsites are gated by `if let Some(t)`.
    #[cfg(test)]
    fn new_for_test(
        app: Arc<RwLock<App>>,
        config: Config,
        message_rx: Receiver<TuiMessage>,
        message_tx: Sender<TuiCommand>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        Ok(Self {
            app,
            config,
            terminal: None,
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
                match event::read()? {
                    Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        if !self.handle_key_event(key).await? {
                            info!("TUI exit: user quit (key={:?})", key.code);
                            break;
                        }
                    }
                    Event::Mouse(mouse_event) => {
                        let mut app = self.app.write().await;
                        app.handle_mouse_event(mouse_event).await?;
                    }
                    Event::FocusLost => {
                        info!("TUI received FocusLost event");
                    }
                    _ => {}
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
        info!(
            "TUI main loop completed (frames={}, msgs={})",
            self.metrics.frames_rendered, self.metrics.messages_processed
        );
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
                    if matches!(message, TuiMessage::DecodedMessage(_)) {
                        info!("TUI process_messages: received DecodedMessage");
                    }
                    self.handle_message(message).await?;
                    message_count += 1;
                    self.metrics.messages_processed += 1;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    warn!("TUI message channel disconnected — UI will continue without live data");
                    break;
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
                let _ = app.add_decoded_message(decoded).await;
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
            TuiMessage::DxSpot {
                callsign,
                frequency,
                spotter: _,
            } => {
                // For now use FT8 as default mode
                app.add_dx_spot(callsign, frequency as f64, "FT8".to_string(), 0);
            }
            TuiMessage::Error {
                component: _,
                message,
            } => {
                app.add_error_message(message);
            }
            TuiMessage::StatusUpdate { component, status } => {
                app.update_component_status(component, status);
            }
            TuiMessage::WaterfallUpdate { rows } => {
                app.push_waterfall_rows(rows);
            }
            TuiMessage::SpotGroupUpdate { spots } => {
                app.merge_spot_groups(&spots);
            }
            TuiMessage::AudioLevel { level } => {
                app.audio_level = level;
            }
            TuiMessage::PipelineHealth(health) => {
                app.pipeline_health = Some(health);
            }
            TuiMessage::ActiveQsosUpdate { qsos } => {
                app.active_qsos = qsos;
            }
        }

        Ok(())
    }

    /// Handle keyboard events
    async fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        let mut app = self.app.write().await;

        // If quit-confirm modal is visible, route keys there
        if app.quit_confirm_visible {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    app.quit_confirm_visible = false;
                    let _ = self.message_tx.send(TuiCommand::Quit);
                    return Ok(false);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    app.quit_confirm_visible = false;
                    app.status_message = "Quit cancelled".to_string();
                }
                _ => {} // swallow all other keys
            }
            return Ok(true);
        }

        // If help overlay is visible, route keys to help handler
        if app.help_visible {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    app.toggle_help();
                }
                _ => {} // swallow all other keys while help is open
            }
            return Ok(true);
        }

        // If device selection modal is visible, route keys there
        if app.device_selection.visible {
            match key.code {
                KeyCode::Esc => {
                    app.device_selection.visible = false;
                    app.status_message = "Device selection cancelled".to_string();
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    app.device_selection.toggle_panel();
                }
                KeyCode::Up => {
                    app.device_selection.move_up();
                }
                KeyCode::Down => {
                    app.device_selection.move_down();
                }
                KeyCode::Enter => {
                    let input = app.device_selection.selected_input_name();
                    let output = app.device_selection.selected_output_name();
                    app.device_selection.visible = false;

                    let msg = format!(
                        "Devices: IN={} OUT={}",
                        input.as_deref().unwrap_or("(none)"),
                        output.as_deref().unwrap_or("(none)")
                    );
                    app.status_message = msg;

                    self.message_tx.send(TuiCommand::SelectDevice {
                        input_device: input,
                        output_device: output,
                    })?;
                }
                _ => {}
            }
            return Ok(true);
        }

        match key.code {
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
                app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(100.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }
            KeyCode::Right => {
                app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(3000.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }

            // TX frequency offset: [ = down 50 Hz, ] = up 50 Hz
            KeyCode::Char('[') => {
                app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(100.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }
            KeyCode::Char(']') => {
                app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(3000.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }

            // Band switching: = = band up (+ dropped; Shift not required),
            // - / _ = band down
            KeyCode::Char('=') => {
                let freq_hz = app.band_up();
                self.message_tx.send(TuiCommand::SetFrequency {
                    vfo: 0,
                    frequency: freq_hz,
                })?;
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                let freq_hz = app.band_down();
                self.message_tx.send(TuiCommand::SetFrequency {
                    vfo: 0,
                    frequency: freq_hz,
                })?;
            }

            // === Quit (with confirm modal) ===
            KeyCode::Char('q') => {
                app.quit_confirm_visible = true;
                app.status_message =
                    "Quit pancetta? Press y/Enter to confirm, n/Esc/q to cancel".to_string();
            }

            // === Modal shortcuts ===
            KeyCode::Char('d') => {
                app.device_selection.visible = true;
                if app.device_selection.input_devices.is_empty()
                    && app.device_selection.output_devices.is_empty()
                {
                    app.status_message =
                        "No audio devices reported — check coordinator connection".to_string();
                } else {
                    app.status_message =
                        "Select audio devices (Tab to switch, Enter to confirm, Esc to cancel)"
                            .to_string();
                }
            }
            KeyCode::Char('?') => {
                app.toggle_help();
            }

            // === CQ + QSO actions ===
            KeyCode::Char('c') => {
                self.message_tx.send(TuiCommand::StartCq)?;
            }
            KeyCode::Char('s') => {
                self.message_tx.send(TuiCommand::StopCq)?;
            }
            KeyCode::Char('h') => {
                // h - Halt current TX. Releases PTT within ~150ms; pancetta
                // keeps running and listening.
                self.message_tx.send(TuiCommand::StopTx)?;
            }
            KeyCode::Char('p') => {
                self.message_tx.send(TuiCommand::TogglePtt)?;
            }

            // === Tune / clear-offset (case-sensitive) ===
            KeyCode::Char('T') => {
                // Shift-T: 12-second single-tone tune. Shift requirement is a
                // small barrier against accidental TX during keyboard fumbling.
                self.message_tx.send(TuiCommand::ToggleTune)?;
            }
            KeyCode::Char('t') => {
                // Lowercase t: find clear TX offset and jump the cursor there.
                match app.find_clear_offset() {
                    Some(hz) => {
                        app.tx_frequency_offset = hz;
                        app.status_message = format!("TX cursor → {:.0} Hz (clear)", hz);
                    }
                    None => {
                        app.status_message = "No clear offset found in your parity".to_string();
                    }
                }
            }

            // === Autonomous controls ===
            KeyCode::Char('a') => {
                app.toggle_autonomous();
            }
            KeyCode::Char('P') => {
                // Shift-P: pause/resume autonomous (uppercase to disambiguate from p=PTT).
                app.toggle_autonomous_pause();
            }
            KeyCode::Char('m') => {
                app.toggle_monitoring().await?;
            }

            // === Display / housekeeping ===
            KeyCode::Char('x') => {
                app.clear_messages();
                self.message_tx.send(TuiCommand::ClearMessages)?;
            }

            // Space - Select/activate (click-to-call)
            KeyCode::Char(' ') => {
                if let Some((callsign, frequency, dx_parity)) = app.get_selected_station() {
                    self.message_tx.send(TuiCommand::CallStation {
                        callsign,
                        frequency,
                        dx_parity,
                    })?;
                }
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

            // Text input (catch-all — must come after all explicit Char arms)
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

        let Some(terminal) = self.terminal.as_mut() else {
            // Headless mode (unit tests) — nothing to render.
            return Ok(());
        };
        terminal.draw(|f| {
            // Use the rich ui::draw for the full frame
            if let Err(e) = crate::ui::draw(f, &app) {
                // Fallback: render a minimal error message
                let error_text = format!("Render error: {}", e);
                let paragraph = Paragraph::new(error_text).style(Style::default().fg(Color::Red));
                f.render_widget(paragraph, f.area());
            }

            // Render device selection modal overlay if visible
            if app.device_selection.visible {
                TuiRunner::render_device_selection_modal(f, f.area(), &app.device_selection);
            }

            // Render help overlay if visible
            if app.help_visible {
                TuiRunner::render_help_overlay(f, f.area());
            }

            // Render quit-confirm overlay if visible (drawn last so it sits on top)
            if app.quit_confirm_visible {
                TuiRunner::render_quit_confirm_overlay(f, f.area());
            }
        })?;

        self.metrics.frames_rendered += 1;
        Ok(())
    }

    /// Render device selection modal as an overlay
    fn render_device_selection_modal(f: &mut Frame, area: Rect, state: &DeviceSelectionState) {
        // Modal dimensions: roughly 60% width, height to fit content
        let modal_width = (area.width * 3 / 5).clamp(40, 70);
        let modal_height = {
            let max_devices = state.input_devices.len().max(state.output_devices.len());
            // title(1) + border(2) + header(1) + devices + footer(2) + border
            (max_devices as u16 + 7).min(area.height - 2).max(10)
        };

        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        // Clear background behind modal
        f.render_widget(ratatui::widgets::Clear, modal_area);

        let outer_block = Block::default()
            .title(" Audio Device Selection ")
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .style(Style::default().bg(Color::Black).fg(Color::White));

        let inner = outer_block.inner(modal_area);
        f.render_widget(outer_block, modal_area);

        // Split inner area: two side-by-side panels + footer
        let vert_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // Device lists
                Constraint::Length(2), // Footer / help text
            ])
            .split(inner);

        let panel_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(vert_chunks[0]);

        // --- Input devices panel ---
        let input_border_style = if state.active_panel == DevicePanel::Input {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let input_items: Vec<ListItem> = state
            .input_devices
            .iter()
            .enumerate()
            .map(|(i, (name, is_default))| {
                let marker = if *is_default { " *" } else { "" };
                let label = format!("{}{}", name, marker);
                let style =
                    if i == state.selected_input_idx && state.active_panel == DevicePanel::Input {
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else if i == state.selected_input_idx {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default()
                    };
                ListItem::new(label).style(style)
            })
            .collect();

        let input_list = List::new(input_items).block(
            Block::default()
                .title(" Input ")
                .borders(Borders::ALL)
                .border_style(input_border_style),
        );
        f.render_widget(input_list, panel_chunks[0]);

        // --- Output devices panel ---
        let output_border_style = if state.active_panel == DevicePanel::Output {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let output_items: Vec<ListItem> = state
            .output_devices
            .iter()
            .enumerate()
            .map(|(i, (name, is_default))| {
                let marker = if *is_default { " *" } else { "" };
                let label = format!("{}{}", name, marker);
                let style = if i == state.selected_output_idx
                    && state.active_panel == DevicePanel::Output
                {
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else if i == state.selected_output_idx {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                };
                ListItem::new(label).style(style)
            })
            .collect();

        let output_list = List::new(output_items).block(
            Block::default()
                .title(" Output ")
                .borders(Borders::ALL)
                .border_style(output_border_style),
        );
        f.render_widget(output_list, panel_chunks[1]);

        // --- Footer help text ---
        let footer =
            Paragraph::new(" Tab: switch panel | Up/Down: select | Enter: confirm | Esc: cancel")
                .style(Style::default().fg(Color::DarkGray));
        f.render_widget(footer, vert_chunks[1]);
    }

    /// Render help overlay as a centered modal
    fn render_help_overlay(f: &mut Frame, area: Rect) {
        let lines: &[(&str, &str)] = &[
            ("?", "Toggle this help"),
            ("Tab / Shift+Tab", "Switch panel"),
            ("Up / Down", "Scroll list"),
            ("Left / Right", "TX offset −/+ 50 Hz"),
            ("[ / ]", "TX offset −/+ 50 Hz"),
            ("= / -", "Band up / down"),
            ("Space", "Call selected station"),
            ("Enter", "Send TX message"),
            ("c / s", "Start / stop CQ"),
            ("t", "Find clear TX offset"),
            ("Shift+T", "Tune (12 s tone)"),
            ("h", "Halt current TX"),
            ("p", "Toggle PTT"),
            ("a", "Toggle autonomous mode"),
            ("Shift+P", "Pause / resume autonomous"),
            ("m", "Toggle audio monitoring"),
            ("d", "Device picker"),
            ("x", "Clear decoded messages"),
            ("q", "Quit (with confirm)"),
            ("Esc", "Dismiss overlay / cancel modal"),
        ];

        // Modal sizing: wide enough for content, tall enough for all lines
        let modal_width: u16 = 52;
        let modal_height = lines.len() as u16 + 5; // lines + title + 2 blank + footer + borders

        let modal_width = modal_width.min(area.width.saturating_sub(4));
        let modal_height = modal_height.min(area.height.saturating_sub(4));

        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        // Clear the area behind the modal
        f.render_widget(ratatui::widgets::Clear, modal_area);

        let outer_block = Block::default()
            .title(" Pancetta Help ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(Color::Black).fg(Color::Cyan));

        let inner = outer_block.inner(modal_area);
        f.render_widget(outer_block, modal_area);

        // Build lines for the Paragraph: blank, bindings, blank, footer
        use ratatui::text::{Line, Span, Text};

        let mut text_lines: Vec<Line> = Vec::new();
        text_lines.push(Line::from(""));

        for (key, desc) in lines {
            let key_span = Span::styled(
                format!("  {:<20}", key),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            let desc_span = Span::styled(*desc, Style::default().fg(Color::White));
            text_lines.push(Line::from(vec![key_span, desc_span]));
        }

        text_lines.push(Line::from(""));
        text_lines.push(Line::from(vec![Span::styled(
            "  Press Escape or ? to close",
            Style::default().fg(Color::DarkGray),
        )]));

        let paragraph = Paragraph::new(Text::from(text_lines))
            .style(Style::default().bg(Color::Black))
            .wrap(Wrap { trim: false });

        f.render_widget(paragraph, inner);
    }

    /// Render quit-confirm overlay as a centered modal
    fn render_quit_confirm_overlay(f: &mut Frame, area: Rect) {
        use ratatui::text::{Line, Span};

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Quit pancetta?  [y/N]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  y / Enter = quit    n / Esc / q = cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let modal_width: u16 = 50;
        let modal_height: u16 = lines.len() as u16 + 2;
        let modal_width = modal_width.min(area.width.saturating_sub(4));
        let modal_height = modal_height.min(area.height.saturating_sub(4));
        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        f.render_widget(ratatui::widgets::Clear, modal_area);

        let block = Block::default()
            .title(" Confirm Quit ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(Color::Black).fg(Color::Red));

        let para = Paragraph::new(lines).block(block);
        f.render_widget(para, modal_area);
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
        let Some(terminal) = self.terminal.as_mut() else {
            // Headless mode (unit tests) — nothing to clean up.
            return Ok(());
        };
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

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

#[cfg(test)]
mod key_tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crossbeam_channel::unbounded;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    async fn make_runner() -> (
        TuiRunner,
        crossbeam_channel::Receiver<TuiCommand>,
        Arc<RwLock<App>>,
    ) {
        let app = Arc::new(RwLock::new(
            App::new(Config::default(), None).await.unwrap(),
        ));
        let (_tui_msg_tx, tui_msg_rx) = unbounded::<TuiMessage>();
        let (cmd_tx, cmd_rx) = unbounded::<TuiCommand>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let runner = TuiRunner::new_for_test(
            Arc::clone(&app),
            Config::default(),
            tui_msg_rx,
            cmd_tx,
            shutdown,
        )
        .unwrap();
        (runner, cmd_rx, app)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn key_shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[tokio::test]
    async fn key_c_emits_start_cq() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('c')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StartCq)));
    }

    #[tokio::test]
    async fn key_s_emits_stop_cq() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('s')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StopCq)));
    }

    #[tokio::test]
    async fn key_h_emits_stop_tx() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('h')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StopTx)));
    }

    #[tokio::test]
    async fn key_p_emits_toggle_ptt() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('p')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::TogglePtt)));
    }

    #[tokio::test]
    async fn key_uppercase_t_emits_toggle_tune() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key_shift('T')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::ToggleTune)));
    }

    #[tokio::test]
    async fn key_lowercase_t_does_not_emit_toggle_tune() {
        // Lowercase t is FindClearOffset (handled locally; no command sent).
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('t')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_q_opens_modal_does_not_quit() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('q')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "must not send Quit yet");
        assert!(
            app.read().await.quit_confirm_visible,
            "modal must be visible"
        );
    }

    #[tokio::test]
    async fn key_y_in_modal_confirms_quit() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('y')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Quit)));
    }

    #[tokio::test]
    async fn key_n_in_modal_dismisses() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('n')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "must not Quit");
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_esc_in_modal_dismisses() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_q_in_modal_dismisses() {
        // Pressing q again while modal is up should dismiss.
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('q')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_enter_in_modal_confirms() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Quit)));
    }

    #[tokio::test]
    async fn key_d_lowercase_opens_device_picker() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('d')).await.unwrap();
        assert!(app.read().await.device_selection.visible);
    }

    #[tokio::test]
    async fn key_d_uppercase_no_longer_opens_device_picker() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_key_event(key_shift('D')).await.unwrap();
        assert!(!app.read().await.device_selection.visible);
    }

    #[tokio::test]
    async fn key_x_emits_clear_messages() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('x')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::ClearMessages)));
    }

    #[tokio::test]
    async fn key_f4_no_longer_does_anything() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_ctrl_q_no_longer_quits() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err(), "Ctrl-Q must no longer quit");
    }

    #[tokio::test]
    async fn key_esc_does_not_quit_when_no_modal() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_equals_emits_band_up() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('=')).await.unwrap();
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(TuiCommand::SetFrequency { .. })
        ));
    }

    #[tokio::test]
    async fn key_plus_no_longer_changes_band() {
        // Spec drops `+` so we don't require Shift.
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('+')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
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
    let app = Arc::new(RwLock::new(App::new(config.clone(), None).await?));

    // Create and run TUI runner
    let runner = TuiRunner::new(app, config, message_rx, message_tx, shutdown)?;
    runner.run().await
}
