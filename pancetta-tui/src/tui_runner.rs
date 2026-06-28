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
    /// Operator-presence clock (FCC §97.221): Unix-epoch milliseconds of the
    /// last key the operator pressed at the console. Stamped on every key event
    /// and read by the coordinator's autonomous-initiation gate. Shared with the
    /// coordinator; a fresh standalone atomic in unit tests.
    last_input_ms: Arc<std::sync::atomic::AtomicU64>,
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
    /// DXCC entity still needed (cqdx needed set), computed in the bridge
    /// poller against the shared `CachedStationLookup`. Inert when cqdx
    /// supplies no needed set.
    pub needed: bool,
    /// Entity is an ATNO (all-time new one). Subset of `needed`.
    pub atno: bool,
}

/// Messages received by the TUI
#[derive(Debug, Clone)]
pub enum TuiMessage {
    /// Decoded FT8 message
    DecodedMessage(DecodedMessageView),
    /// Frequency update
    FrequencyUpdate { vfo: u8, frequency: u64 },
    /// Rig S-meter update. Value follows the hamlib STRENGTH
    /// convention: dB relative to S9 (0 = S9, -54 ≈ S0, +20 = S9+20).
    /// Produced by the coordinator's rig polling loop (Batch 95); only
    /// real rig readings arrive here — never synthesized data.
    SignalStrengthUpdate { db_over_s9: i32 },
    /// Live SWR reading (e.g. 1.3 = 1.3:1) from the rig while keyed. Shown in
    /// the status bar only during TX. Real `\get_level SWR` reads.
    SwrUpdate { swr: f32 },
    /// DX spot
    DxSpot {
        callsign: String,
        frequency: u64,
        spotter: String,
        /// Worked-before flag computed by the coordinator relay against
        /// the same CachedStationLookup the autonomous scorer uses
        /// (band-scoped on the spot frequency, uppercase-exact match).
        worked_before: bool,
        /// DXCC entity still needed (cqdx needed set). Inert when off.
        needed: bool,
        /// Entity is an ATNO (all-time new one). Subset of `needed`.
        atno: bool,
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
    /// owns the truth, receiver is a passive renderer. Batch 94:
    /// also rebuilds the QSO-detail panel entries (`qso_statuses`)
    /// from the same snapshot via `App::apply_active_qsos`.
    /// `pending_calls` carries the cross-parity manual-call queue
    /// (#40) in the same push so the TUI sees a consistent
    /// (active, queued) pair.
    ActiveQsosUpdate {
        qsos: Vec<crate::app::ActiveQsoBanner>,
        pending_calls: Vec<crate::app::PendingCallBanner>,
    },
    /// Structured autonomous-operator status, forwarded by the
    /// coordinator's relay from the autonomous loop (one per 15s
    /// slot). Replaces the old flattened status-bar-text-only path —
    /// the relay still sends the text line too (additive), but this
    /// is what drives the live `[AUTO]` panel in station_info.
    AutonomousStatusUpdate(crate::app::AutonomousStatus),
    /// TX-active indicator. `active: true` is sent by the coordinator's
    /// TX worker when PTT is asserted; `false` when the transmission
    /// ends — normal completion, operator abort (F8 / Shift+Q), or
    /// shutdown all clear it. Drives the title-bar " TX " badge.
    TxStatus { active: bool },
    /// Richer TX-queue snapshot: what is on the air RIGHT NOW and what is
    /// queued for an upcoming slot. Forwarded by the coordinator relay from
    /// the TX worker's `TxQueueStatus`. Drives the "TX" NOW/QUEUED lines.
    TxQueueUpdate {
        /// Message + frequency being transmitted now (`None` = idle).
        sending: Option<crate::app::TxQueueItem>,
        /// Items dequeued/scheduled but not yet on the air.
        queued: Vec<crate::app::TxQueueItem>,
    },
    /// Current global tri-state TX policy. Echoed by the coordinator relay
    /// whenever the operator changes it (cycle key `g`, or Shift+Q →
    /// Disabled). Drives the bold, color-coded TX-policy banner.
    TxPolicyUpdate {
        /// New global TX policy.
        policy: pancetta_core::TxPolicy,
    },
    /// Split-TX frequency echo. Sent by the coordinator relay after every
    /// write to the split atomic: modal set, manual band-change clear,
    /// and autonomous band-hop clear. `tx_hz == 0` means simplex (chip
    /// hidden). Drives the "SPLIT TX" title-bar chip authoritatively.
    SplitUpdate {
        /// Current split TX frequency in Hz, or 0 for simplex.
        tx_hz: u64,
    },
    /// Available audio devices, enumerated by the coordinator (which owns
    /// the `pancetta-audio` host) and pushed to the TUI once at startup so
    /// the `d` device-selection picker can list them. Each entry is
    /// `(name, is_default)`. The TUI is a passive renderer — it never
    /// enumerates hardware itself.
    DeviceListUpdate {
        input: Vec<(String, bool)>,
        output: Vec<(String, bool)>,
        current_output: Option<String>,
    },
    /// Rig connection state for the station-panel badge. Pushed by the
    /// coordinator relay from the hamlib connect/poll loop.
    RigStatusUpdate {
        /// Current rig connection state.
        state: crate::app::RigConnDisplay,
    },
    /// Whether TX audio is routed to the system default output rather than an
    /// explicit rig CODEC (the "PTT keys, audio on speakers" misconfig).
    /// Drives a persistent station-panel warning badge.
    AudioOutputDefault {
        /// `true` = system-default fallback (misconfig).
        is_default: bool,
    },
}

/// Commands sent from TUI
#[derive(Debug, Clone)]
pub enum TuiCommand {
    /// Change frequency
    SetFrequency { vfo: u8, frequency: u64 },
    /// Enable/disable rig split (RX dial ≠ TX dial). `tx_frequency` is the
    /// split TX dial in Hz (ignored when `enabled == false`).
    SetSplit { enabled: bool, tx_frequency: u64 },
    /// Start CQ
    StartCq {
        /// Operator's TX audio offset (Hz) from the waterfall cursor.
        frequency_offset: f64,
    },
    /// Stop CQ
    StopCq,
    /// Send message
    SendMessage {
        text: String,
        /// Operator's TX audio offset (Hz) from the waterfall cursor.
        frequency_offset: f64,
    },
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
    /// Reply to a station calling US (from the Callers panel), opening the
    /// exchange at the operator-chosen sequence step (smart default +
    /// override) rather than always sending our grid.
    RespondToCaller {
        /// The caller's callsign.
        callsign: String,
        /// Audio offset (Hz) where we heard them / will reply.
        frequency: u64,
        /// The slot parity the caller transmits on, if known.
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        /// Which rung of the exchange ladder to open at.
        step: pancetta_core::ResponseStep,
        /// Our measured SNR of the caller (drives the report we send).
        snr: Option<f32>,
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
    /// Operator pressed the emergency-stop key (`Q` / `q` with Shift).
    /// hb-161: Phase 5 safety driver. One keypress halts everything the
    /// station is doing without exiting pancetta. The coordinator's
    /// command-forwarding task aborts any in-flight TX, disables
    /// autonomous mode at runtime, stops the repeating-CQ loop and any
    /// active tune tone, and logs the event at WARN with
    /// `target: "operator.override"`.
    ///
    /// Distinct from:
    /// - `Quit` (whole-app shutdown via the `q` confirm modal)
    /// - `StopTx` (halt current TX only; autonomous keeps running)
    /// - `StopCq` (turn off repeating CQ only)
    OperatorEmergencyStop,
    /// Operator pressed `a` — toggle the autonomous runtime gate.
    /// The coordinator flips the SAME `autonomous_enabled_runtime`
    /// flag that `OperatorEmergencyStop` clears, so Shift+Q → `a`
    /// is the documented safety-recovery path: emergency stop
    /// disables autonomous TX, `a` re-enables it. Re-enabling never
    /// starts a transmission directly — it only re-opens the gate
    /// the autonomous decision loop checks before dispatching TX
    /// (which has its own slot/priority/QSO gates).
    ToggleAutonomous,
    /// Operator pressed `k` — abort the currently selected QSO. The
    /// coordinator cancels it (→ Failed{UserCancelled}, callsign mapping
    /// cleared). No-op when no QSO is selected.
    AbortQso { qso_id: String },
    /// Operator pressed `r` — re-send the most recent message we
    /// transmitted in the selected QSO. No-op when no QSO is selected.
    ResendQso { qso_id: String },
    /// Operator pressed `g` — cycle the global tri-state TX policy
    /// (Full → RespondOnly → Disabled → Full). The coordinator updates
    /// the shared `tx_policy` atomic and echoes the new state back as a
    /// `TxPolicyUpdate` for the bold TX banner. RespondOnly suppresses all
    /// new initiations (CQ, hunting) while keeping in-progress QSOs and
    /// answering callers; Disabled is a hard RX-only mute.
    CycleTxPolicy,
    /// Operator pressed `f`: toggle the TX-frequency mode Hold ↔ Auto. The
    /// coordinator flips the shared `tx_freq_mode` atomic. Hold (default) keeps
    /// the operator's picked offset sticky; Auto lets pancetta choose/adjust it
    /// (smart allocator, collision jitter, stuck-DX hop).
    ToggleTxFreqMode,
    /// Operator used the `o` modal to set (or clear) the held TX audio offset.
    /// `Some(hz)` → store `hz` into `tx_offset_hold_hz` AND flip
    /// `tx_freq_mode` to `Hold` so the offset is actually used.
    /// `None` → store `0` into `tx_offset_hold_hz` AND flip `tx_freq_mode` to
    /// `Auto` (clear / release the offset).
    SetTxOffset {
        /// The desired held TX audio offset in Hz, or `None` to clear (→ Auto).
        offset_hz: Option<u64>,
    },
    /// Operator pressed `H` (Shift+H) on the DX Hunter panel to engage Hound
    /// mode on the selected Fox station. The coordinator opens a manual Hound
    /// QSO: calls the Fox low (300–900 Hz), QSYs up (>1000 Hz) when the Fox
    /// answers, and completes on RR73. TX-policy gated (initiation, same as
    /// `CallStation`).
    ///
    /// Note: lowercase `h` is already bound to `StopTx` (halt TX), so Hound
    /// engage uses `Shift+H` as the spec's documented fallback.
    EngageHound {
        /// The Fox's callsign.
        callsign: String,
        /// The Fox's RX audio offset (Hz) — where we hear the Fox; becomes the
        /// `partner_freq` for the relevance gate.
        fox_freq: u64,
        /// The Fox's slot parity (we TX on the opposite). `None` when unknown.
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        /// The Fox's Maidenhead grid square, if known (logging only).
        fox_grid: Option<String>,
    },
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
        last_input_ms: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Self> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        // Force a full repaint on the first draw. Without this, ratatui's
        // first frame diffs against its default-blank buffer and skips any
        // cell that matches the default (e.g. regions a widget leaves
        // unpainted), so the terminal's pre-launch scrollback bleeds through
        // the alternate screen. clear() resets the back buffer so every cell
        // is written on the next draw.
        terminal.clear()?;

        Ok(Self {
            app,
            config,
            terminal: Some(terminal),
            message_rx,
            message_tx,
            shutdown,
            last_input_ms,
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
            last_input_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

            // Perf (Pass 1 / infra-A6): snapshot the cumulative message count
            // so the adaptive timeout below can test RECENT activity (this
            // iteration) rather than lifetime total. The old test
            // `messages_processed > 0` is monotonic — true forever after the
            // first message — which pinned the poll timeout at 10ms for the
            // whole session and defeated the intended 50ms idle cadence.
            let msgs_before = self.metrics.messages_processed;

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

                // FCC §97.221 presence prompt: if autonomous mode is enabled but
                // the operator has been idle past the presence window, the
                // coordinator is suppressing autonomous initiation. Flag it so
                // the title bar prompts the operator to press a key. Mirrors the
                // coordinator's OPERATOR_PRESENCE_WINDOW (120 s).
                {
                    const PRESENCE_WINDOW_MS: u64 = 120_000;
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let last = self
                        .last_input_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let stale = last == 0 || now_ms.saturating_sub(last) >= PRESENCE_WINDOW_MS;
                    let mut app = self.app.write().await;
                    let auto_on = app
                        .autonomous_status
                        .as_ref()
                        .map(|s| s.enabled)
                        .unwrap_or(false);
                    app.autonomous_init_paused = auto_on && stale;
                }

                self.render_frame().await?;

                let render_time = render_start.elapsed();
                self.update_metrics(render_time);

                self.last_render = Instant::now();
            } else {
                // Small yield to prevent busy waiting
                tokio::time::sleep(Duration::from_millis(1)).await;
            }

            // Adaptive timeout based on RECENT activity (messages processed this
            // iteration), not lifetime total — so an idle TUI actually falls
            // back to the 50ms cadence instead of spinning at 10ms forever.
            event_timeout = if self.metrics.messages_processed > msgs_before {
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
            TuiMessage::SignalStrengthUpdate { db_over_s9 } => {
                app.update_signal_strength(db_over_s9);
            }
            TuiMessage::SwrUpdate { swr } => {
                app.update_swr(swr);
            }
            TuiMessage::DxSpot {
                callsign,
                frequency,
                spotter: _,
                worked_before,
                needed,
                atno,
            } => {
                // For now use FT8 as default mode
                app.add_dx_spot(
                    callsign,
                    frequency as f64,
                    "FT8".to_string(),
                    0,
                    worked_before,
                    needed,
                    atno,
                );
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
            TuiMessage::ActiveQsosUpdate {
                qsos,
                pending_calls,
            } => {
                app.apply_active_qsos(qsos, pending_calls);
            }
            TuiMessage::AutonomousStatusUpdate(status) => {
                app.update_autonomous_status(status);
            }
            TuiMessage::TxStatus { active } => {
                app.is_transmitting = active;
            }
            TuiMessage::TxQueueUpdate { sending, queued } => {
                app.tx_now_sending = sending;
                app.tx_queued = queued;
            }
            TuiMessage::TxPolicyUpdate { policy } => {
                app.tx_policy = policy;
            }
            TuiMessage::SplitUpdate { tx_hz } => {
                app.split_tx_hz = tx_hz;
            }
            TuiMessage::DeviceListUpdate {
                input,
                output,
                current_output,
            } => {
                app.set_audio_devices(input, output, current_output);
            }
            TuiMessage::RigStatusUpdate { state } => {
                app.rig_connected = state;
            }
            TuiMessage::AudioOutputDefault { is_default } => {
                app.tx_output_default = is_default;
            }
        }

        Ok(())
    }

    /// Handle keyboard events
    async fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // Operator-presence stamp (FCC §97.221): ANY key the operator presses is
        // proof they are at the console, which lets the autonomous engine
        // initiate contact for the presence window. Stamp before routing so even
        // navigation/no-op keys count as presence.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_input_ms
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);

        let mut app = self.app.write().await;

        // Required out-of-band acknowledgment modal — must be dismissed first.
        if app.out_of_band_ack_visible {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    app.out_of_band_ack_visible = false;
                    app.status_message = "Out-of-band TX acknowledged".to_string();
                }
                _ => {}
            }
            return Ok(true);
        }

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

        // Frequency-entry modal (Shift+F): two MHz text fields.
        if app.freq_modal.visible {
            match key.code {
                KeyCode::Esc => {
                    app.freq_modal.visible = false;
                    app.status_message = "Frequency entry cancelled".to_string();
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    app.freq_modal.field = match app.freq_modal.field {
                        crate::app::FreqModalField::RxDial => crate::app::FreqModalField::TxSplit,
                        crate::app::FreqModalField::TxSplit => crate::app::FreqModalField::RxDial,
                    };
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => match app.freq_modal.field {
                    crate::app::FreqModalField::RxDial => app.freq_modal.rx_buffer.push(c),
                    crate::app::FreqModalField::TxSplit => app.freq_modal.tx_buffer.push(c),
                },
                KeyCode::Backspace => match app.freq_modal.field {
                    crate::app::FreqModalField::RxDial => {
                        app.freq_modal.rx_buffer.pop();
                    }
                    crate::app::FreqModalField::TxSplit => {
                        app.freq_modal.tx_buffer.pop();
                    }
                },
                KeyCode::Enter => {
                    let rx_hz = crate::app::parse_mhz_to_hz(&app.freq_modal.rx_buffer);
                    let tx_hz = crate::app::parse_mhz_to_hz(&app.freq_modal.tx_buffer); // None = simplex
                    match rx_hz {
                        None => {
                            app.status_message =
                                "Invalid RX dial — enter MHz e.g. 14.085".to_string();
                        }
                        Some(rx) => {
                            app.freq_modal.visible = false;
                            self.message_tx.send(TuiCommand::SetFrequency {
                                vfo: 0,
                                frequency: rx,
                            })?;
                            let (split_enabled, split_freq) = match tx_hz {
                                Some(tx) => (true, tx),
                                None => (false, 0),
                            };
                            app.split_tx_hz = split_freq;
                            self.message_tx.send(TuiCommand::SetSplit {
                                enabled: split_enabled,
                                tx_frequency: split_freq,
                            })?;
                            // Out-of-band check on the effective TX RF + current offset.
                            let tx_dial = if split_enabled { split_freq } else { rx };
                            let tx_rf = tx_dial + app.tx_frequency_offset as u64;
                            if crate::app::tx_rf_out_of_us_band(tx_rf) && !app.out_of_band_warned {
                                app.out_of_band_warned = true;
                                app.out_of_band_ack_visible = true;
                                app.out_of_band_rf_hz = tx_rf;
                            }
                            app.status_message = if split_enabled {
                                format!(
                                    "Dial {:.3} MHz, SPLIT TX {:.3} MHz",
                                    rx as f64 / 1e6,
                                    split_freq as f64 / 1e6
                                )
                            } else {
                                format!("Dial {:.3} MHz (simplex)", rx as f64 / 1e6)
                            };
                            app.freq_modal.rx_buffer.clear();
                            app.freq_modal.tx_buffer.clear();
                            app.freq_modal.field = crate::app::FreqModalField::RxDial;
                        }
                    }
                }
                _ => {}
            }
            return Ok(true);
        }

        // TX-audio-offset modal (`o`): single integer Hz field, range 200–2900.
        // Blank Enter = clear → Auto; valid number → SetTxOffset{Some(hz)};
        // out-of-range → reject with status message; Esc = cancel.
        if app.offset_modal.visible {
            match key.code {
                KeyCode::Esc => {
                    app.offset_modal.visible = false;
                    app.status_message = "TX offset entry cancelled".to_string();
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    app.offset_modal.buffer.push(c);
                }
                KeyCode::Backspace => {
                    app.offset_modal.buffer.pop();
                }
                KeyCode::Enter => {
                    let trimmed = app.offset_modal.buffer.trim().to_string();
                    if trimmed.is_empty() {
                        // Blank → clear held offset → Auto
                        app.offset_modal.visible = false;
                        app.offset_modal.buffer.clear();
                        app.tx_offset_hold_hz = None;
                        app.tx_freq_mode = pancetta_core::TxFreqMode::Auto;
                        app.status_message = "TX offset auto (Tx=Rx)".to_string();
                        self.message_tx
                            .send(TuiCommand::SetTxOffset { offset_hz: None })?;
                    } else {
                        match crate::app::parse_hz(&trimmed) {
                            Some(hz)
                                if hz >= crate::app::TX_OFFSET_MIN_HZ
                                    && hz <= crate::app::TX_OFFSET_MAX_HZ =>
                            {
                                app.offset_modal.visible = false;
                                app.offset_modal.buffer.clear();
                                app.tx_offset_hold_hz = Some(hz);
                                app.tx_freq_mode = pancetta_core::TxFreqMode::Hold;
                                app.status_message =
                                    format!("TX offset held @ {} Hz", hz);
                                self.message_tx
                                    .send(TuiCommand::SetTxOffset { offset_hz: Some(hz) })?;
                            }
                            _ => {
                                app.status_message = format!(
                                    "Invalid offset — enter {} to {} Hz (blank = Auto)",
                                    crate::app::TX_OFFSET_MIN_HZ,
                                    crate::app::TX_OFFSET_MAX_HZ
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
            return Ok(true);
        }

        // Compose (free-text TX) mode. While active, the keyboard is a text
        // editor — Char/Backspace edit the TX buffer, Enter sends + exits, Esc
        // cancels + exits. Command letters are intentionally inert here so the
        // operator can type a callsign/message without firing c/s/k/g. This is
        // the explicit command-vs-text-input split (UX audit Batch 1).
        if app.compose_mode {
            match key.code {
                KeyCode::Esc => {
                    app.cancel_compose_mode();
                }
                KeyCode::Enter => {
                    let text = app.get_input_text();
                    if text.trim().is_empty() {
                        app.cancel_compose_mode();
                    } else {
                        self.message_tx.send(TuiCommand::SendMessage {
                            text: text.clone(),
                            frequency_offset: app.tx_frequency_offset,
                        })?;
                        app.clear_input();
                        app.compose_mode = false;
                        app.status_message = format!("TX sent: {}", text);
                    }
                }
                KeyCode::Backspace => {
                    app.delete_char();
                }
                KeyCode::Char(c) => {
                    app.input_char(c);
                }
                _ => {} // swallow other keys while composing
            }
            return Ok(true);
        }

        match key.code {
            // hb-161: Esc clears the operator-stop banner without re-enabling
            // anything. Re-enabling autonomous still requires `a`. Bound here
            // (not in a modal-style early-return block) so other keys keep
            // working — the banner is informational, not blocking.
            KeyCode::Esc if app.stopped_by_operator => {
                app.stopped_by_operator = false;
                app.status_message = "Operator-stop banner cleared".to_string();
            }

            // Panel navigation
            KeyCode::Tab => {
                app.next_panel();
            }
            KeyCode::BackTab => {
                app.previous_panel();
            }

            // Arrow keys for list navigation. When the QSO Status panel is
            // focused, Up/Down move the QSO selection cursor instead (which
            // drives the abort/re-send target).
            KeyCode::Up => {
                if matches!(app.active_panel, crate::app::ActivePanel::QsoStatus) {
                    app.qso_cursor_up();
                } else {
                    app.previous_item();
                    // Moving the Callers selection resets the reply override
                    // so each newly-selected caller starts at its smart default.
                    if matches!(app.active_panel, crate::app::ActivePanel::Callers) {
                        app.clamp_callers_selection();
                    }
                }
            }
            KeyCode::Down => {
                if matches!(app.active_panel, crate::app::ActivePanel::QsoStatus) {
                    app.qso_cursor_down();
                } else {
                    app.next_item();
                    if matches!(app.active_panel, crate::app::ActivePanel::Callers) {
                        app.clamp_callers_selection();
                    }
                }
            }
            // Jump/page navigation for the focused list (Band Activity is
            // newest-first, so Home = back to realtime — no more holding Up).
            // `,`/`<` and `.`/`>` duplicate Home/End for keyboards without
            // dedicated Home/End keys (e.g. the iPad on-screen / Magic Keyboard).
            // `<` (left) = back to newest/realtime, `>` (right) = oldest. Both the
            // shifted glyphs and their unshifted `,`/`.` bases are bound so the
            // operator never needs a modifier.
            KeyCode::Home | KeyCode::Char(',') | KeyCode::Char('<') => {
                app.scroll_to_top();
                app.status_message = "Jumped to newest (realtime)".to_string();
            }
            KeyCode::End | KeyCode::Char('.') | KeyCode::Char('>') => {
                app.scroll_to_bottom();
                app.status_message = "Jumped to oldest".to_string();
            }
            KeyCode::PageUp => {
                app.page_up();
            }
            KeyCode::PageDown => {
                app.page_down();
            }
            // Left/Right are globally TX-offset ±50 Hz, BUT when the Callers
            // panel is focused they cycle the reply sequence step for the
            // selected caller instead (so the operator can override the smart
            // default without leaving the panel).
            KeyCode::Left => {
                if matches!(app.active_panel, crate::app::ActivePanel::Callers) {
                    app.cycle_caller_reply(false);
                    let step = app.current_caller_reply_step();
                    app.status_message = format!("Reply step: {:?}", step);
                } else {
                    app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(200.0);
                    app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
                }
            }
            KeyCode::Right => {
                if matches!(app.active_panel, crate::app::ActivePanel::Callers) {
                    app.cycle_caller_reply(true);
                    let step = app.current_caller_reply_step();
                    app.status_message = format!("Reply step: {:?}", step);
                } else {
                    app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(2500.0);
                    app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
                }
            }

            // TX frequency offset: [ = down 50 Hz, ] = up 50 Hz. Clamped to
            // 200–2500 Hz to match the modulator/passband and find_clear_offset
            // — below 200 Hz multi-TX offsets go negative and single-TX
            // silently encode-rejects.
            KeyCode::Char('[') => {
                app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(200.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }
            KeyCode::Char(']') => {
                app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(2500.0);
                app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
            }

            // Band switching: = = band up (+ dropped; Shift not required),
            // - / _ = band down
            KeyCode::Char('=') => {
                let freq_hz = app.band_up();
                app.split_tx_hz = 0; // band change reverts rig to simplex (coordinator clears atomic+rig)
                self.message_tx.send(TuiCommand::SetFrequency {
                    vfo: 0,
                    frequency: freq_hz,
                })?;
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                let freq_hz = app.band_down();
                app.split_tx_hz = 0; // band change reverts rig to simplex (coordinator clears atomic+rig)
                self.message_tx.send(TuiCommand::SetFrequency {
                    vfo: 0,
                    frequency: freq_hz,
                })?;
            }

            // Panel jump 1-5 (advertised in help; map matches the historic
            // app.rs binding). 1=Band, 2=QSO, 3=Station, 4=Callers, 5=DX.
            KeyCode::Char('1') => {
                app.active_panel = crate::app::ActivePanel::BandActivity;
                app.status_message = "Panel: Band Activity".to_string();
            }
            KeyCode::Char('2') => {
                app.active_panel = crate::app::ActivePanel::QsoStatus;
                app.status_message = "Panel: QSO Status".to_string();
            }
            KeyCode::Char('3') => {
                app.active_panel = crate::app::ActivePanel::StationInfo;
                app.status_message = "Panel: Station Info".to_string();
            }
            KeyCode::Char('4') => {
                app.active_panel = crate::app::ActivePanel::Callers;
                app.status_message = "Panel: Callers".to_string();
            }
            KeyCode::Char('5') => {
                app.active_panel = crate::app::ActivePanel::DxHunter;
                app.status_message = "Panel: DX Hunter".to_string();
            }

            // === Quit (with confirm modal) ===
            KeyCode::Char('q') => {
                app.quit_confirm_visible = true;
                app.status_message =
                    "Quit pancetta? Press y/Enter to confirm, n/Esc/q to cancel".to_string();
            }

            // === Emergency stop (hb-161: Phase 5 safety driver) ===
            // Shift+Q halts the station without exiting. Distinct from
            // lowercase `q` (quit-confirm). The coordinator handles the
            // event: aborts in-flight TX, disables autonomous at runtime,
            // stops the repeating CQ + active tune, and logs at WARN.
            // The TUI also flips `stopped_by_operator` locally so the
            // banner appears immediately — no round-trip needed for the
            // visual signal.
            KeyCode::Char('Q') => {
                app.stopped_by_operator = true;
                // Emergency stop hard-mutes TX: reflect Disabled in the local
                // banner immediately; the coordinator confirms via
                // TxPolicyUpdate.
                app.tx_policy = pancetta_core::TxPolicy::Disabled;
                app.status_message =
                    "STOPPED BY OPERATOR — autonomous off, TX aborted (press Esc to clear banner)"
                        .to_string();
                warn!(
                    target: "operator.override",
                    "Operator pressed Q emergency stop key — halting station"
                );
                self.message_tx.send(TuiCommand::OperatorEmergencyStop)?;
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
                self.message_tx.send(TuiCommand::StartCq {
                    frequency_offset: app.tx_frequency_offset,
                })?;
            }
            KeyCode::Char('s') => {
                self.message_tx.send(TuiCommand::StopCq)?;
            }
            KeyCode::Char('h') => {
                // h - Halt current TX. Releases PTT within ~150ms; pancetta
                // keeps running and listening.
                self.message_tx.send(TuiCommand::StopTx)?;
            }
            // Shift+H — Engage Hound mode on the selected DX-Hunter station.
            // Lowercase `h` is taken by StopTx (halt TX), so Hound uses H.
            // Only active when the DX Hunter panel is focused; a no-op hint
            // is shown on other panels so the operator knows to switch.
            KeyCode::Char('H') => {
                if matches!(app.active_panel, crate::app::ActivePanel::DxHunter) {
                    // Pull callsign + fox_freq + parity from the selected row,
                    // and also the grid (one extra field beyond get_selected_station).
                    let selected = app.displayed_dx_stations();
                    if let Some(station) = selected.get(app.dx_hunter_scroll) {
                        let callsign = station.call_sign.clone();
                        let fox_freq = station.audio_offset_hz.unwrap_or(1500);
                        let dx_parity = station.slot_parity;
                        let fox_grid = station.grid_square.clone();
                        self.message_tx.send(TuiCommand::EngageHound {
                            callsign: callsign.clone(),
                            fox_freq,
                            dx_parity,
                            fox_grid,
                        })?;
                        app.status_message = format!("Hound: engaging {} as Fox...", callsign);
                    } else {
                        app.status_message = "No DX station selected for Hound".to_string();
                    }
                } else {
                    app.status_message =
                        "Focus DX Hunter (H key) to engage Hound on selected station".to_string();
                }
            }
            KeyCode::Char('p') => {
                // TX-policy safety gate (mirror of the relay's authoritative
                // check): refuse keying PTT while TX is Disabled. The relay
                // can't tell a key-up from a key-down across the toggle, but
                // the common operator intent for `p` after a Shift+Q is to
                // key up — so we hard-refuse here for instant feedback and the
                // relay enforces the real ON-transition gate. PTT-OFF is never
                // blocked there.
                if !app.tx_policy.allows_any_tx() {
                    app.status_message =
                        "Can't key PTT — TX is DISABLED (press g to re-enable)".to_string();
                } else {
                    self.message_tx.send(TuiCommand::TogglePtt)?;
                }
            }

            // === QSO management (operate on the selected QSO) ===
            // r / k are gated to the QSO Status panel so the operator can't
            // accidentally re-send or abort the selected QSO while scrolling
            // Band Activity or DX Hunter (the historic foot-gun: `k` aborting a
            // QSO from an unrelated panel). Both echo the target callsign.
            // r - re-send our most recent message in the selected QSO.
            KeyCode::Char('r')
                if matches!(app.active_panel, crate::app::ActivePanel::QsoStatus) =>
            {
                match (app.selected_qso_id(), app.selected_qso_callsign()) {
                    (Some(qso_id), Some(call)) => {
                        self.message_tx.send(TuiCommand::ResendQso { qso_id })?;
                        app.status_message = format!("Re-sending last TX to {}", call);
                    }
                    _ => {
                        app.status_message = "No active QSO to re-send".to_string();
                    }
                }
            }
            // k - kill/abort the selected QSO.
            KeyCode::Char('k')
                if matches!(app.active_panel, crate::app::ActivePanel::QsoStatus) =>
            {
                match (app.selected_qso_id(), app.selected_qso_callsign()) {
                    (Some(qso_id), Some(call)) => {
                        self.message_tx.send(TuiCommand::AbortQso { qso_id })?;
                        app.status_message = format!("Aborting QSO with {}", call);
                    }
                    _ => {
                        app.status_message = "No active QSO to abort".to_string();
                    }
                }
            }
            // r / k pressed outside the QSO Status panel: hint, don't act.
            KeyCode::Char('r') | KeyCode::Char('k') => {
                app.status_message =
                    "Focus the QSO Status panel (2) to re-send (r) / abort (k)".to_string();
            }

            // === Tune / clear-offset (case-sensitive) ===
            KeyCode::Char('T') => {
                // Shift-T: 12-second single-tone tune. Shift requirement is a
                // small barrier against accidental TX during keyboard fumbling.
                // TX-policy safety gate: a tune carrier is a transmission, so
                // refuse it while TX is Disabled (RX-only). A tune can never be
                // in flight while Disabled — cycling to Disabled aborts any
                // active tune — so blocking the toggle wholesale is safe.
                if !app.tx_policy.allows_any_tx() {
                    app.status_message =
                        "Can't tune — TX is DISABLED (press g to re-enable)".to_string();
                } else {
                    self.message_tx.send(TuiCommand::ToggleTune)?;
                }
            }
            KeyCode::Char('t') => {
                // Lowercase t: jump the cursor to the best TX offset. Always
                // picks something (least-congested if nothing is truly clear).
                match app.find_clear_offset() {
                    Some((hz, true)) => {
                        app.tx_frequency_offset = hz;
                        app.status_message = format!("TX cursor → {:.0} Hz (clear)", hz);
                    }
                    Some((hz, false)) => {
                        app.tx_frequency_offset = hz;
                        app.status_message =
                            format!("TX cursor → {:.0} Hz (best available — band is busy)", hz);
                    }
                    None => {
                        app.status_message = "No TX offset available".to_string();
                    }
                }
            }

            // === Autonomous controls ===
            KeyCode::Char('a') => {
                // Flip local state optimistically for immediate feedback,
                // then send the toggle to the coordinator — it flips the
                // authoritative `autonomous_enabled_runtime` gate (the
                // same one Shift+Q clears) and the next live
                // AutonomousStatusUpdate confirms/corrects the panel.
                // Also clear the operator-stop banner: pressing `a` is
                // the documented re-engagement action after Shift+Q.
                app.toggle_autonomous();
                if app.stopped_by_operator {
                    app.stopped_by_operator = false;
                }
                self.message_tx.send(TuiCommand::ToggleAutonomous)?;
            }
            KeyCode::Char('P') => {
                // Shift-P: pause/resume autonomous (uppercase to disambiguate from p=PTT).
                app.toggle_autonomous_pause();
            }

            // === Global TX policy (tri-state) ===
            // g - cycle Full → RespondOnly → Disabled → Full. Optimistically
            // flip the local banner for instant feedback; the coordinator
            // echoes the authoritative state back via TxPolicyUpdate.
            KeyCode::Char('g') => {
                let next = app.tx_policy.cycle();
                app.tx_policy = next;
                app.status_message = format!("TX policy: {}", next.label());
                self.message_tx.send(TuiCommand::CycleTxPolicy)?;
            }
            // f - toggle TX-frequency mode Hold ↔ Auto. Hold (default) keeps the
            // offset you picked sticky; Auto lets pancetta choose/adjust it.
            // Optimistically flip the local chip; the coordinator flips the
            // authoritative shared atomic.
            KeyCode::Char('f') => {
                let next = app.tx_freq_mode.toggle();
                app.tx_freq_mode = next;
                app.status_message = format!("TX freq: {}", next.label());
                self.message_tx.send(TuiCommand::ToggleTxFreqMode)?;
            }
            // Shift+F — open the arbitrary-frequency / split entry modal.
            KeyCode::Char('F') => {
                app.freq_modal.visible = true;
                app.freq_modal.field = crate::app::FreqModalField::RxDial;
                app.freq_modal.rx_buffer.clear();
                app.freq_modal.tx_buffer.clear();
                app.status_message = "Freq entry: dial MHz, Tab→split, Enter, Esc".to_string();
            }
            // `o` — open the TX-audio-offset modal. Prompts for an integer Hz
            // value in [200, 2900]; blank Enter = clear (→ Auto). Mirrors the
            // Shift+F pattern but for audio offsets within the passband.
            KeyCode::Char('o') => {
                app.offset_modal.visible = true;
                app.offset_modal.buffer.clear();
                app.status_message =
                    "TX offset: enter Hz (200–2900), blank=Auto, Enter, Esc".to_string();
            }
            KeyCode::Char('m') => {
                app.toggle_monitoring().await?;
            }

            // === Compose free-text TX ===
            // `/` enters compose mode (a visible TX input line). Outside
            // compose, letters are commands only — this is the single entry
            // point for typing a free-text message so command keys never fire
            // mid-typing.
            KeyCode::Char('/') => {
                app.enter_compose_mode();
            }

            // === Display / housekeeping ===
            KeyCode::Char('x') => {
                app.clear_messages();
                self.message_tx.send(TuiCommand::ClearMessages)?;
            }

            // Space - context-aware action on the selected station ("do the
            // right next thing"). If the selected callsign most-recently sent
            // us something directed at us (grid/report/R/RR73/73), reply at the
            // correct exchange step (same smart-default the Callers panel uses);
            // otherwise answer their CQ with our grid. This unifies Space with
            // Callers-Enter and fixes "I clicked to send 73 but it sent my
            // grid" — pressing Space on a station still sending us RR73 sends
            // another 73.
            KeyCode::Char(' ') => match app.resolve_space_action() {
                Some(crate::app::SpaceAction::Reply {
                    callsign,
                    frequency,
                    dx_parity,
                    step,
                    snr,
                }) => {
                    self.message_tx.send(TuiCommand::RespondToCaller {
                        callsign: callsign.clone(),
                        frequency,
                        dx_parity,
                        step,
                        snr,
                    })?;
                    app.status_message = format!(
                        "Replying {} to {}",
                        crate::app::reply_step_label(step),
                        callsign
                    );
                }
                Some(crate::app::SpaceAction::Call {
                    callsign,
                    frequency,
                    dx_parity,
                }) => {
                    self.message_tx.send(TuiCommand::CallStation {
                        callsign: callsign.clone(),
                        frequency,
                        dx_parity,
                    })?;
                    app.status_message = format!("Calling {}...", callsign);
                }
                None => {
                    app.status_message = "No station selected".to_string();
                }
            },

            // Enter - confirm the selected caller reply. When the Callers panel
            // is focused and a caller is selected, this commits the reply at the
            // shown sequence step. Free-text TX now lives in compose mode (`/`),
            // so outside Callers there's nothing for Enter to send — it hints
            // the operator toward compose instead.
            KeyCode::Enter => {
                if matches!(app.active_panel, crate::app::ActivePanel::Callers) {
                    if let Some((callsign, frequency, dx_parity)) = app.get_selected_station() {
                        let step = app.current_caller_reply_step();
                        let snr = app.selected_caller().map(|m| m.snr as f32);
                        self.message_tx.send(TuiCommand::RespondToCaller {
                            callsign: callsign.clone(),
                            frequency,
                            dx_parity,
                            step,
                            snr,
                        })?;
                        app.status_message = format!("Replying to {} ({:?})", callsign, step);
                    } else {
                        app.status_message = "No caller selected".to_string();
                    }
                } else {
                    app.status_message = "Press / to compose a free-text TX message".to_string();
                }
            }

            // NOTE: there is intentionally NO `Char(c)` text catch-all here.
            // Outside compose mode every letter is a command; free-text input
            // is reachable only via `/` (compose mode), so a stray keystroke
            // can never silently fill the TX buffer or fire a command while the
            // operator means to type.
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

            // Render operator-stop banner if active. Drawn before the
            // quit-confirm overlay so the latter sits on top — Q-press
            // then q-press should still let the operator quit.
            if app.stopped_by_operator {
                TuiRunner::render_stopped_by_operator_banner(f, f.area());
            }

            // Render quit-confirm overlay if visible (drawn last so it sits on top)
            if app.quit_confirm_visible {
                TuiRunner::render_quit_confirm_overlay(f, f.area());
            }

            // Render freq/split/offset modals (out-of-band ack has priority;
            // then freq modal; then offset modal — only one visible at a time).
            if app.out_of_band_ack_visible {
                crate::ui::render_out_of_band_modal(f, f.area(), app.out_of_band_rf_hz);
            } else if app.freq_modal.visible {
                crate::ui::render_freq_modal(f, f.area(), &app.freq_modal);
            } else if app.offset_modal.visible {
                crate::ui::render_offset_modal(f, f.area(), &app.offset_modal);
            }
        })?;

        self.metrics.frames_rendered += 1;
        Ok(())
    }

    /// Render device selection modal as an overlay
    fn render_device_selection_modal(f: &mut Frame, area: Rect, state: &DeviceSelectionState) {
        // Defensive: a zero/near-zero area (terminal not yet sized, or a
        // remote session reporting 0×0 at launch) makes every dimension
        // computation below underflow. In release builds an unchecked
        // `area.height - 2` wraps to a huge u16 → out-of-bounds render →
        // SIGBUS ("bus error") instead of a clean panic. Skip the overlay
        // entirely until there's room for it; the base UI's min-size guard
        // already shows the "terminal too small" notice.
        if area.width < 10 || area.height < 4 {
            return;
        }
        // Modal dimensions: roughly 60% width, height to fit content
        let modal_width = (area.width * 3 / 5).clamp(40, 70).min(area.width);
        let modal_height = {
            let max_devices = state.input_devices.len().max(state.output_devices.len());
            // title(1) + border(2) + header(1) + devices + footer(2) + border
            (max_devices as u16 + 7)
                .min(area.height.saturating_sub(2))
                .max(10)
                .min(area.height)
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
            (
                "Home / End (or < / >)",
                "Jump to newest (realtime) / oldest",
            ),
            ("PgUp / PgDn", "Page scroll"),
            ("1/2/3/4/5", "Jump: Band/QSO/Station/Callers/DX"),
            (
                "Left / Right",
                "TX offset −/+ 50 Hz (Callers: cycle reply step)",
            ),
            ("[ / ]", "TX offset −/+ 50 Hz"),
            ("= / -", "Band up / down"),
            ("Space", "Call selected station"),
            ("/", "Compose free-text TX (Enter sends, Esc cancels)"),
            ("Enter", "Callers: reply at shown step"),
            ("c / s", "Start / stop CQ"),
            ("k", "Abort selected QSO (QSO Status panel only)"),
            ("r", "Re-send last TX (QSO Status panel only)"),
            ("t", "Find clear TX offset (auto-pick + pin)"),
            (
                "f",
                "TX freq mode: HOLD (pin offset) / AUTO (pancetta picks)",
            ),
            ("o", "Set TX audio offset Hz (blank=Auto) — implies Hold"),
            (
                "Shift+F",
                "Set dial / split freq (RX MHz + optional TX MHz)",
            ),
            ("Shift+T", "Tune (12 s tone; blocked while TX DISABLED)"),
            ("h", "Halt current TX"),
            ("p", "Toggle PTT (blocked while TX DISABLED)"),
            ("a", "Toggle autonomous mode"),
            ("Shift+P", "Pause / resume autonomous"),
            ("m", "Toggle audio monitoring"),
            ("d", "Device picker"),
            ("x", "Clear decoded messages"),
            ("q", "Quit (with confirm)"),
            ("Shift+Q", "EMERGENCY STOP (halt TX, autonomous off)"),
            ("Esc", "Dismiss overlay / cancel modal / clear stop banner"),
        ];

        // Modal sizing: size to the widest "  <key:24><desc>" line so nothing
        // wraps, then cap to the available width. The old fixed 52-col width
        // forced long descriptions (and the 21-char "Home / End (or < / >)"
        // key) to wrap, making the overlay hard to read.
        const KEY_COL: usize = 24; // ≥ longest key (21) + gap
        let widest_line = lines
            .iter()
            .map(|(_, desc)| 2 + KEY_COL + desc.chars().count())
            .max()
            .unwrap_or(50);
        // +2 for the left/right borders.
        let modal_width = ((widest_line + 2) as u16).min(area.width.saturating_sub(2));
        let modal_height = lines.len() as u16 + 5; // lines + title + 2 blank + footer + borders
        let modal_height = modal_height.min(area.height.saturating_sub(2));

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
                format!("  {:<width$}", key, width = KEY_COL),
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

    /// hb-161: render the "STOPPED BY OPERATOR" banner across the top of
    /// the frame. Non-modal — the operator can still interact with the
    /// rest of the TUI, but the red banner makes it impossible to miss
    /// that the emergency-stop is in effect. Cleared by Esc.
    fn render_stopped_by_operator_banner(f: &mut Frame, area: Rect) {
        use ratatui::text::{Line, Span};

        // Two-row banner pinned to the top of the screen. Tall enough to
        // be unmistakable, short enough not to obliterate the main view.
        let banner_height: u16 = 3;
        let banner_height = banner_height.min(area.height);
        let banner_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: banner_height,
        };

        f.render_widget(ratatui::widgets::Clear, banner_area);

        let lines = vec![
            Line::from(Span::styled(
                "  STOPPED BY OPERATOR — autonomous off, TX aborted",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  Press Esc to clear banner. Press `a` to re-enable autonomous.",
                Style::default().fg(Color::White).bg(Color::Red),
            )),
        ];

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .style(Style::default().bg(Color::Red).fg(Color::White));

        let para = Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(Color::Red).fg(Color::White));
        f.render_widget(para, banner_area);
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

/// Create and run TUI with message bus integration
#[allow(clippy::too_many_arguments)]
pub async fn run_tui_with_message_bus(
    config: Config,
    message_rx: Receiver<TuiMessage>,
    message_tx: Sender<TuiCommand>,
    shutdown: Arc<AtomicBool>,
    last_input_ms: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    // Create app state
    let app = Arc::new(RwLock::new(App::new(config.clone(), None).await?));

    // Create and run TUI runner
    let runner = TuiRunner::new(app, config, message_rx, message_tx, shutdown, last_input_ms)?;
    runner.run().await
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
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StartCq { .. })));
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

    /// Regression: the device-selection modal must not underflow its
    /// dimension math at a tiny/zero terminal area. Pre-fix, the
    /// `area.height - 2` subtraction underflowed → debug panic / release
    /// SIGBUS ("bus error" on launch over a remote session reporting 0×0).
    /// Render at a range of degenerate sizes and assert no panic.
    #[test]
    fn device_modal_renders_without_underflow_at_tiny_sizes() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut state = DeviceSelectionState::new();
        state.input_devices = vec![("USB CODEC".into(), true), ("Jump Audio".into(), false)];
        state.output_devices = vec![("USB CODEC".into(), true)];
        state.visible = true;

        for (w, h) in [(1u16, 1u16), (0, 0), (3, 1), (10, 2), (40, 3), (80, 24)] {
            let backend = TestBackend::new(w.max(1), h.max(1));
            let mut terminal = Terminal::new(backend).unwrap();
            // Must not panic (subtract-with-overflow) at any size.
            terminal
                .draw(|f| {
                    TuiRunner::render_device_selection_modal(f, f.area(), &state);
                })
                .unwrap();
        }
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

    // === hb-161: Q STOP key emergency operator override ===

    /// Shift+Q emits OperatorEmergencyStop AND flips the
    /// `stopped_by_operator` banner flag immediately so the operator
    /// sees the keypress register without waiting for the coordinator
    /// round trip.
    #[tokio::test]
    async fn key_shift_q_emits_emergency_stop_and_sets_banner() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key_shift('Q')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::OperatorEmergencyStop)),
            "Shift+Q must emit OperatorEmergencyStop"
        );
        assert!(
            app.read().await.stopped_by_operator,
            "Shift+Q must flip the banner state"
        );
    }

    /// Lowercase q must NOT emit OperatorEmergencyStop — it's reserved
    /// for the quit-confirm modal. Regression guard against accidentally
    /// re-binding lowercase q while shipping the safety driver.
    #[tokio::test]
    async fn key_lowercase_q_does_not_emit_emergency_stop() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('q')).await.unwrap();
        // The quit-confirm modal should be visible, NOT the operator-stop banner.
        assert!(
            app.read().await.quit_confirm_visible,
            "lowercase q must open quit-confirm modal"
        );
        assert!(
            !app.read().await.stopped_by_operator,
            "lowercase q must not flip the operator-stop banner"
        );
        let cmd = cmd_rx.try_recv();
        assert!(
            !matches!(cmd, Ok(TuiCommand::OperatorEmergencyStop)),
            "lowercase q must not emit OperatorEmergencyStop (got {:?})",
            cmd
        );
    }

    /// Esc clears the operator-stop banner. The banner is informational,
    /// not modal, so other keys keep working even while it's visible —
    /// but Esc is the documented dismissal.
    #[tokio::test]
    async fn key_esc_clears_operator_stop_banner() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        app.write().await.stopped_by_operator = true;
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            !app.read().await.stopped_by_operator,
            "Esc must clear the banner"
        );
    }

    /// While the operator-stop banner is up, regular keys still work.
    /// The banner is a warning, not a modal — other interactions (e.g.
    /// switching panels) keep functioning so the operator can inspect
    /// the state of the system.
    #[tokio::test]
    async fn banner_visible_does_not_block_other_keys() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.stopped_by_operator = true;
        r.handle_key_event(key('c')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::StartCq { .. })),
            "c key must still emit StartCq even with banner visible"
        );
    }

    // === Batch 93: autonomous toggle + live status + TX indicator ===

    /// `a` must emit ToggleAutonomous to the coordinator — the local
    /// state flip alone is not enough (that was the pre-Batch-93 bug:
    /// the key only mutated TUI-local state and the runtime gate never
    /// moved, so Shift+Q could not be recovered from).
    #[tokio::test]
    async fn key_a_emits_toggle_autonomous() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('a')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::ToggleAutonomous)),
            "a key must emit ToggleAutonomous"
        );
    }

    /// Safety-recovery path (Shift+Q → a): pressing `a` while the
    /// operator-stop banner is up clears the banner AND emits the
    /// toggle so the coordinator re-opens the runtime gate.
    #[tokio::test]
    async fn key_a_clears_operator_stop_banner_and_emits_toggle() {
        let (mut r, cmd_rx, app) = make_runner().await;
        // Simulate the emergency stop having happened.
        app.write().await.stopped_by_operator = true;
        r.handle_key_event(key('a')).await.unwrap();
        assert!(
            !app.read().await.stopped_by_operator,
            "a must clear the operator-stop banner"
        );
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::ToggleAutonomous)),
            "a must emit ToggleAutonomous after an emergency stop"
        );
    }

    /// AutonomousStatusUpdate populates `app.autonomous_status` so the
    /// `[AUTO]` panel renders from live data instead of staying on the
    /// muted "Disabled" placeholder forever.
    #[tokio::test]
    async fn autonomous_status_update_populates_app_state() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        assert!(app.read().await.autonomous_status.is_none());
        let status = crate::app::AutonomousStatus {
            enabled: true,
            state: "Hunting".to_string(),
            slot_parity: Some("Even".to_string()),
            listen_counter: "2/5".to_string(),
            active_qsos: 1,
            max_qsos: 3,
            idle_cycles: 0,
            band_name: "20m".to_string(),
            tx_offset_hz: 1500.0,
        };
        r.handle_message(TuiMessage::AutonomousStatusUpdate(status))
            .await
            .unwrap();
        let app = app.read().await;
        let live = app.autonomous_status.as_ref().expect("status must be set");
        assert!(live.enabled);
        assert_eq!(live.state, "Hunting");
        assert_eq!(live.active_qsos, 1);
        assert_eq!(live.band_name, "20m");
    }

    /// Batch 94: ActiveQsosUpdate feeds BOTH the banner list and the
    /// QSO-detail panel; an empty follow-up snapshot clears both (this
    /// is how completed/failed QSOs leave the panel).
    #[tokio::test]
    async fn active_qsos_update_drives_detail_panel() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        let banner = crate::app::ActiveQsoBanner {
            their_callsign: "JA1ABC".to_string(),
            state: "wait rpt".to_string(),
            started_at: chrono::Utc::now(),
            frequency_hz: 1500.0,
            tx_parity: None,
            last_tx_text: Some("JA1ABC K5ARH EM10".to_string()),
            last_tx_at: Some(chrono::Utc::now()),
            last_rx_text: Some("K5ARH JA1ABC -12".to_string()),
            last_rx_at: Some(chrono::Utc::now()),
            snr_rx: Some(-12),
            report_sent: Some(-8),
            report_received: Some(-15),
            exchange_count: 2,
            qso_id: "33333333-3333-3333-3333-333333333333".to_string(),
            initiated_by: "Manual".to_string(),
            ladder_labels: vec!["Grid".to_string(), "Rpt".to_string()],
            ladder_ours: vec![true, false],
            ladder_index: 1,
            now_line: "waiting".to_string(),
            next_line: "their signal report".to_string(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
            dx_last_activity: None,
            hound: false,
        };
        r.handle_message(TuiMessage::ActiveQsosUpdate {
            qsos: vec![banner],
            pending_calls: Vec::new(),
        })
        .await
        .unwrap();
        {
            let app = app.read().await;
            assert_eq!(app.active_qsos.len(), 1, "banner list populated");
            assert_eq!(app.qso_statuses.len(), 1, "detail panel populated");
            let q = &app.qso_statuses[0];
            assert_eq!(q.call_sign.as_deref(), Some("JA1ABC"));
            assert_eq!(q.state.as_deref(), Some("wait rpt"));
            assert_eq!(q.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        }

        // Completed/failed QSOs vanish from the next snapshot → panel clears.
        r.handle_message(TuiMessage::ActiveQsosUpdate {
            qsos: Vec::new(),
            pending_calls: Vec::new(),
        })
        .await
        .unwrap();
        let app = app.read().await;
        assert!(app.active_qsos.is_empty());
        assert!(app.qso_statuses.is_empty());
    }

    /// TxStatus drives `app.is_transmitting` (the title-bar " TX "
    /// badge) — true lights it, false clears it.
    #[tokio::test]
    async fn tx_status_sets_and_clears_is_transmitting() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        assert!(!app.read().await.is_transmitting);
        r.handle_message(TuiMessage::TxStatus { active: true })
            .await
            .unwrap();
        assert!(app.read().await.is_transmitting, "TX badge must light");
        r.handle_message(TuiMessage::TxStatus { active: false })
            .await
            .unwrap();
        assert!(!app.read().await.is_transmitting, "TX badge must clear");
    }

    /// `g` emits CycleTxPolicy and optimistically advances the local banner
    /// state Full → RespondOnly.
    #[tokio::test]
    async fn key_g_cycles_tx_policy() {
        let (mut r, cmd_rx, app) = make_runner().await;
        assert_eq!(
            app.read().await.tx_policy,
            pancetta_core::TxPolicy::Full,
            "default policy is Full"
        );
        r.handle_key_event(key('g')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::CycleTxPolicy)),
            "g must emit CycleTxPolicy"
        );
        assert_eq!(
            app.read().await.tx_policy,
            pancetta_core::TxPolicy::RespondOnly,
            "local banner advances optimistically"
        );
    }

    /// TxPolicyUpdate (coordinator echo) drives the authoritative banner.
    #[tokio::test]
    async fn tx_policy_update_sets_banner() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_message(TuiMessage::TxPolicyUpdate {
            policy: pancetta_core::TxPolicy::Disabled,
        })
        .await
        .unwrap();
        assert_eq!(
            app.read().await.tx_policy,
            pancetta_core::TxPolicy::Disabled
        );
    }

    /// `SplitUpdate` (coordinator echo) drives the authoritative SPLIT TX chip.
    /// A non-zero tx_hz sets the chip; zero clears it (simplex).
    #[tokio::test]
    async fn split_update_sets_chip() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        // Default is simplex (0).
        assert_eq!(app.read().await.split_tx_hz, 0, "default is simplex");
        // Coordinator pushes a split freq → chip lights.
        r.handle_message(TuiMessage::SplitUpdate { tx_hz: 14_074_000 })
            .await
            .unwrap();
        assert_eq!(
            app.read().await.split_tx_hz,
            14_074_000,
            "SplitUpdate sets split_tx_hz"
        );
        // Coordinator pushes 0 (band-hop / manual clear) → chip clears.
        r.handle_message(TuiMessage::SplitUpdate { tx_hz: 0 })
            .await
            .unwrap();
        assert_eq!(
            app.read().await.split_tx_hz,
            0,
            "SplitUpdate tx_hz=0 clears chip"
        );
    }

    /// TxQueueUpdate populates the NOW-SENDING / QUEUED view.
    #[tokio::test]
    async fn tx_queue_update_populates_view() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_message(TuiMessage::TxQueueUpdate {
            sending: Some(crate::app::TxQueueItem {
                text: "K5ARH JA1ABC -12".to_string(),
                freq_hz: 1500.0,
                qso_id: Some("q1".to_string()),
                deferred: false,
            }),
            queued: vec![crate::app::TxQueueItem {
                text: "CQ K5ARH EM00".to_string(),
                freq_hz: 1200.0,
                qso_id: None,
                deferred: true,
            }],
        })
        .await
        .unwrap();
        let app = app.read().await;
        assert_eq!(
            app.tx_now_sending.as_ref().map(|i| i.text.as_str()),
            Some("K5ARH JA1ABC -12")
        );
        assert_eq!(app.tx_queued.len(), 1);
        assert_eq!(app.tx_queued[0].freq_hz, 1200.0);
    }

    // === UX audit Batch 3 ===========================================

    /// The TX offset clamp matches the modulator/passband (200–2500 Hz):
    /// hammering `[` never goes below 200, hammering `]` never exceeds 2500.
    #[tokio::test]
    async fn tx_offset_clamps_to_modulator_passband() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        // Drive far below the floor with `[` (down 50 Hz each press).
        for _ in 0..200 {
            r.handle_key_event(key('[')).await.unwrap();
        }
        assert_eq!(
            app.read().await.tx_frequency_offset,
            200.0,
            "TX offset must clamp at the 200 Hz floor"
        );
        // Drive far above the ceiling with `]` (up 50 Hz each press).
        for _ in 0..200 {
            r.handle_key_event(key(']')).await.unwrap();
        }
        assert_eq!(
            app.read().await.tx_frequency_offset,
            2500.0,
            "TX offset must clamp at the 2500 Hz ceiling"
        );
    }

    /// The waterfall TX cursor follows the LIVE TX frequency while sending,
    /// and falls back to the manual offset when idle (the 1350→2300 fix lives
    /// in `render_waterfall`; here we assert the App state the renderer reads).
    #[tokio::test]
    async fn waterfall_cursor_prefers_live_tx_freq_when_sending() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        // Idle: cursor source is the manual offset (default 1500).
        {
            let a = app.read().await;
            assert!(a.tx_now_sending.is_none());
            assert_eq!(a.tx_frequency_offset, 1500.0);
        }
        // Now sending at a different live frequency.
        r.handle_message(TuiMessage::TxQueueUpdate {
            sending: Some(crate::app::TxQueueItem {
                text: "K5ARH W1AW -10".to_string(),
                freq_hz: 2300.0,
                qso_id: Some("q1".to_string()),
                deferred: false,
            }),
            queued: vec![],
        })
        .await
        .unwrap();
        let a = app.read().await;
        // The renderer uses tx_now_sending.freq_hz (2300) over the manual
        // offset (still 1500) — the exact state branch the cursor reads.
        assert_eq!(a.tx_now_sending.as_ref().unwrap().freq_hz, 2300.0);
        assert_eq!(a.tx_frequency_offset, 1500.0);
    }

    // === UX audit Batch 1 ===========================================

    /// Build a minimal active-QSO banner for the r/k panel-gating tests.
    fn banner(call: &str, qso_id: &str) -> crate::app::ActiveQsoBanner {
        crate::app::ActiveQsoBanner {
            their_callsign: call.to_string(),
            state: "wait rpt".to_string(),
            started_at: chrono::Utc::now(),
            frequency_hz: 1500.0,
            tx_parity: None,
            last_tx_text: None,
            last_tx_at: None,
            last_rx_text: None,
            last_rx_at: None,
            snr_rx: None,
            report_sent: None,
            report_received: None,
            exchange_count: 0,
            qso_id: qso_id.to_string(),
            initiated_by: "Manual".to_string(),
            ladder_labels: vec![],
            ladder_ours: vec![],
            ladder_index: 0,
            now_line: String::new(),
            next_line: String::new(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
            dx_last_activity: None,
            hound: false,
        }
    }

    /// `p` (PTT) must NOT emit TogglePtt while the local policy mirror is
    /// Disabled — keying the rig there would put a carrier on the air after
    /// Shift+Q / cycle-to-Disabled.
    #[tokio::test]
    async fn key_p_refused_while_tx_disabled() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.tx_policy = pancetta_core::TxPolicy::Disabled;
        r.handle_key_event(key('p')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "p must not emit TogglePtt while TX is Disabled"
        );
        assert!(app.read().await.status_message.contains("Can't key PTT"));
    }

    /// `p` still keys PTT under Full / RespondOnly.
    #[tokio::test]
    async fn key_p_allowed_while_respond_only() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.tx_policy = pancetta_core::TxPolicy::RespondOnly;
        r.handle_key_event(key('p')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::TogglePtt)));
    }

    /// Shift+T (tune) must be refused while TX is Disabled.
    #[tokio::test]
    async fn key_tune_refused_while_tx_disabled() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.tx_policy = pancetta_core::TxPolicy::Disabled;
        r.handle_key_event(key_shift('T')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "Shift+T must not emit ToggleTune while TX is Disabled"
        );
        assert!(app.read().await.status_message.contains("Can't tune"));
    }

    /// `k` aborts ONLY when the QSO Status panel is focused, and echoes the
    /// target callsign.
    #[tokio::test]
    async fn key_k_gated_to_qso_status_panel() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut a = app.write().await;
            a.apply_active_qsos(vec![banner("W1AW", "qso-1")], Vec::new());
            a.active_panel = crate::app::ActivePanel::BandActivity;
        }
        // Wrong panel: no abort, just a hint.
        r.handle_key_event(key('k')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "k must not abort while not on QSO Status panel"
        );

        // Focus QSO Status, then k aborts with callsign echo.
        app.write().await.active_panel = crate::app::ActivePanel::QsoStatus;
        r.handle_key_event(key('k')).await.unwrap();
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(TuiCommand::AbortQso { qso_id }) if qso_id == "qso-1"
        ));
        assert!(app.read().await.status_message.contains("W1AW"));
    }

    /// `r` (re-send) is likewise gated to the QSO Status panel.
    #[tokio::test]
    async fn key_r_gated_to_qso_status_panel() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut a = app.write().await;
            a.apply_active_qsos(vec![banner("K5ARH", "qso-9")], Vec::new());
            a.active_panel = crate::app::ActivePanel::DxHunter;
        }
        r.handle_key_event(key('r')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "r gated off DX Hunter panel");

        app.write().await.active_panel = crate::app::ActivePanel::QsoStatus;
        r.handle_key_event(key('r')).await.unwrap();
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(TuiCommand::ResendQso { qso_id }) if qso_id == "qso-9"
        ));
    }

    /// Digit keys 1-5 jump panels in the production handler (were dead before).
    #[tokio::test]
    async fn digits_jump_panels() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('5')).await.unwrap();
        assert_eq!(
            app.read().await.active_panel,
            crate::app::ActivePanel::DxHunter
        );
        r.handle_key_event(key('2')).await.unwrap();
        assert_eq!(
            app.read().await.active_panel,
            crate::app::ActivePanel::QsoStatus
        );
        r.handle_key_event(key('1')).await.unwrap();
        assert_eq!(
            app.read().await.active_panel,
            crate::app::ActivePanel::BandActivity
        );
    }

    /// `/` enters compose mode; outside compose, command letters fire instead
    /// of feeding the TX buffer.
    #[tokio::test]
    async fn slash_enters_compose_and_routes_text() {
        let (mut r, cmd_rx, app) = make_runner().await;
        // Outside compose: 'c' is the StartCq command, NOT text.
        r.handle_key_event(key('c')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StartCq { .. })));
        assert!(app.read().await.tx_input_buffer.is_empty());

        // Enter compose with '/'.
        r.handle_key_event(key('/')).await.unwrap();
        assert!(app.read().await.compose_mode, "/ enters compose mode");

        // Now letters edit the buffer (uppercased) and no command fires.
        for c in "cq".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        assert!(cmd_rx.try_recv().is_err(), "no command fires in compose");
        assert_eq!(app.read().await.tx_input_buffer, "CQ");
    }

    /// Enter in compose mode sends the buffer and exits compose.
    #[tokio::test]
    async fn compose_enter_sends_and_exits() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('/')).await.unwrap();
        for c in "test".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(TuiCommand::SendMessage { text, .. }) if text == "TEST"
        ));
        assert!(!app.read().await.compose_mode, "Enter exits compose");
        assert!(app.read().await.tx_input_buffer.is_empty());
    }

    /// Esc in compose mode cancels without sending.
    #[tokio::test]
    async fn compose_esc_cancels() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('/')).await.unwrap();
        r.handle_key_event(key('x')).await.unwrap();
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err(), "Esc must not send");
        let a = app.read().await;
        assert!(!a.compose_mode);
        assert!(a.tx_input_buffer.is_empty());
    }

    // ── Hound mode key binding (Shift+H on DX Hunter) ────────────────────────

    /// Pressing Shift+H on the DX Hunter panel with a station selected emits
    /// `EngageHound` carrying the station's callsign, audio offset, parity,
    /// and grid.
    #[tokio::test]
    async fn shift_h_on_dx_hunter_emits_engage_hound() {
        use chrono::Utc;
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut a = app.write().await;
            a.active_panel = crate::app::ActivePanel::DxHunter;
            // Inject a DX station directly into dx_stations so
            // displayed_dx_stations() returns it.
            a.dx_stations.insert(
                "VK9XX".to_string(),
                crate::app::DxStation {
                    call_sign: "VK9XX".to_string(),
                    grid_square: Some("QH30".to_string()),
                    frequency: 14.074,
                    mode: "FT8".to_string(),
                    last_seen: Utc::now(),
                    snr: -10,
                    distance: None,
                    bearing: None,
                    worked_before: false,
                    needed: true,
                    atno: false,
                    priority_score: 800,
                    source: crate::app::SpotSource::Local,
                    entity_name: None,
                    rarity_tier: None,
                    reporter_count: None,
                    is_notable: false,
                    notable_type: None,
                    confidence: None,
                    best_snr_network: None,
                    last_seen_network: None,
                    audio_offset_hz: Some(750),
                    slot_parity: Some(pancetta_core::slot::SlotParity::Even),
                },
            );
            a.dx_hunter_scroll = 0;
        }
        r.handle_key_event(key_shift('H')).await.unwrap();
        match cmd_rx.try_recv() {
            Ok(TuiCommand::EngageHound {
                callsign,
                fox_freq,
                dx_parity,
                fox_grid,
            }) => {
                assert_eq!(callsign, "VK9XX");
                assert_eq!(fox_freq, 750);
                assert_eq!(dx_parity, Some(pancetta_core::slot::SlotParity::Even));
                assert_eq!(fox_grid, Some("QH30".to_string()));
            }
            other => panic!("Expected EngageHound, got {:?}", other),
        }
        assert!(
            app.read().await.status_message.contains("VK9XX"),
            "status message echoes the Fox callsign"
        );
    }

    /// Pressing Shift+H when NOT on the DX Hunter panel emits nothing and
    /// shows a panel-focus hint (no accidental Hound engage from other panels).
    #[tokio::test]
    async fn shift_h_outside_dx_hunter_emits_nothing() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.active_panel = crate::app::ActivePanel::BandActivity;
        r.handle_key_event(key_shift('H')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "Shift+H outside DX Hunter must not emit EngageHound"
        );
        assert!(
            app.read().await.status_message.contains("DX Hunter"),
            "shows a panel-focus hint"
        );
    }

    /// Pressing Shift+H on the DX Hunter when no station is selected emits
    /// nothing and shows an appropriate hint.
    #[tokio::test]
    async fn shift_h_on_empty_dx_hunter_emits_nothing() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut a = app.write().await;
            a.active_panel = crate::app::ActivePanel::DxHunter;
            // Leave dx_stations empty so displayed_dx_stations() is empty.
        }
        r.handle_key_event(key_shift('H')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "Shift+H with no station selected must not emit EngageHound"
        );
        assert!(
            app.read().await.status_message.contains("selected"),
            "shows a 'no station selected' hint"
        );
    }

    // === TX-offset modal (`o` key) =====================================

    /// `o` opens the offset modal and clears the buffer.
    #[tokio::test]
    async fn key_o_opens_offset_modal() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        // Pre-populate buffer from a hypothetical previous open.
        app.write().await.offset_modal.buffer = "1234".to_string();
        r.handle_key_event(key('o')).await.unwrap();
        let a = app.read().await;
        assert!(a.offset_modal.visible, "modal must be visible after 'o'");
        assert!(a.offset_modal.buffer.is_empty(), "buffer cleared on open");
    }

    /// Valid Hz entry on Enter emits `SetTxOffset{Some(hz)}`, sets Hold mode,
    /// and closes the modal.
    #[tokio::test]
    async fn offset_modal_valid_entry_emits_set_tx_offset() {
        let (mut r, cmd_rx, app) = make_runner().await;
        // Open the modal.
        r.handle_key_event(key('o')).await.unwrap();
        assert!(app.read().await.offset_modal.visible);
        // Type "1500".
        for c in "1500".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        // Enter → apply.
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let a = app.read().await;
        assert!(!a.offset_modal.visible, "modal closes on valid Enter");
        assert_eq!(
            a.tx_offset_hold_hz,
            Some(1500),
            "local offset set to 1500"
        );
        assert_eq!(
            a.tx_freq_mode,
            pancetta_core::TxFreqMode::Hold,
            "mode flipped to Hold"
        );
        drop(a);
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::SetTxOffset {
                    offset_hz: Some(1500)
                })
            ),
            "SetTxOffset(Some(1500)) emitted"
        );
    }

    /// Out-of-range entry is rejected; modal stays open with a status message.
    #[tokio::test]
    async fn offset_modal_out_of_range_rejected() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('o')).await.unwrap();
        // Type "100" (below minimum 300).
        for c in "100".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let a = app.read().await;
        assert!(a.offset_modal.visible, "modal stays open on bad input");
        assert!(
            a.status_message.contains("Invalid"),
            "status message says invalid"
        );
        drop(a);
        assert!(
            cmd_rx.try_recv().is_err(),
            "no command emitted for out-of-range"
        );
    }

    /// Values in the old range [200, 299] and [2701, 2900] are now rejected
    /// because they fall outside the coordinator's clamp range [300, 2700].
    #[tokio::test]
    async fn offset_modal_old_range_boundary_rejected() {
        // 200 was valid under the old 200-2900 bounds but must now be rejected.
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('o')).await.unwrap();
        for c in "200".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let a = app.read().await;
        assert!(a.offset_modal.visible, "200 Hz is below new minimum 300 — modal stays open");
        assert!(a.status_message.contains("Invalid"), "status says invalid");
        drop(a);
        assert!(cmd_rx.try_recv().is_err(), "no command emitted");
    }

    /// Blank entry on Enter emits `SetTxOffset{None}` (→ Auto) and closes.
    #[tokio::test]
    async fn offset_modal_blank_entry_clears_to_auto() {
        let (mut r, cmd_rx, app) = make_runner().await;
        // Set a held offset first.
        app.write().await.tx_offset_hold_hz = Some(1500);
        r.handle_key_event(key('o')).await.unwrap();
        // Enter immediately (empty buffer).
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let a = app.read().await;
        assert!(!a.offset_modal.visible, "modal closes on blank Enter");
        assert_eq!(a.tx_offset_hold_hz, None, "offset cleared to None");
        assert_eq!(
            a.tx_freq_mode,
            pancetta_core::TxFreqMode::Auto,
            "mode flipped to Auto"
        );
        drop(a);
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::SetTxOffset { offset_hz: None })
            ),
            "SetTxOffset(None) emitted"
        );
    }

    /// Esc cancels without emitting a command.
    #[tokio::test]
    async fn offset_modal_esc_cancels() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('o')).await.unwrap();
        for c in "1500".chars() {
            r.handle_key_event(key(c)).await.unwrap();
        }
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        let a = app.read().await;
        assert!(!a.offset_modal.visible, "modal closed on Esc");
        drop(a);
        assert!(
            cmd_rx.try_recv().is_err(),
            "Esc must not emit a command"
        );
    }

    /// `parse_hz` unit tests.
    #[test]
    fn parse_hz_accepts_and_rejects() {
        assert_eq!(crate::app::parse_hz("1500"), Some(1500));
        assert_eq!(crate::app::parse_hz("200"), Some(200));
        assert_eq!(crate::app::parse_hz("2900"), Some(2900));
        assert_eq!(crate::app::parse_hz("  1500  "), Some(1500));
        assert_eq!(crate::app::parse_hz(""), None);
        assert_eq!(crate::app::parse_hz("abc"), None);
        assert_eq!(crate::app::parse_hz("-500"), None);
    }
}
