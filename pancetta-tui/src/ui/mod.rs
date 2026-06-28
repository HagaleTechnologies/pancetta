use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::{ActivePanel, App};
use crate::widgets::Waterfall;

pub mod active_qsos;
pub mod band_activity;
pub mod callers;
pub mod dx_hunter;
pub mod qso_status;
pub mod station_info;

use active_qsos::render_active_qsos;
use band_activity::render_band_activity;
use callers::render_callers;
use dx_hunter::render_dx_hunter;
use qso_status::render_qso_status;
use station_info::render_station_info;

/// Main UI rendering function
pub fn draw(f: &mut Frame<'_>, app: &App) -> Result<()> {
    let size = f.area();

    // Paint an opaque full-frame background first so every cell carries an
    // explicit bg. This guarantees the alternate screen fully covers the
    // terminal's pre-launch scrollback even where a widget paints nothing
    // (e.g. an empty waterfall when audio is silent) — those gaps would
    // otherwise show through.
    f.render_widget(
        Block::default().style(Style::default().bg(app.theme.background_color())),
        size,
    );

    // Minimum-size guard. Below this the multi-panel layout degrades into
    // unreadable empty boxes (and panels silently drop content); show an
    // explicit resize prompt instead so a new operator isn't staring at a
    // broken-looking screen wondering what's wrong.
    const MIN_W: u16 = 80;
    const MIN_H: u16 = 20;
    if size.width < MIN_W || size.height < MIN_H {
        let msg = vec![
            Line::from(Span::styled(
                "Terminal too small",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "Have {}x{}, need at least {}x{}.",
                size.width, size.height, MIN_W, MIN_H
            )),
            Line::from("Resize the window (or your SSH/terminal) and the UI returns."),
        ];
        let p = Paragraph::new(msg)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true });
        // Vertically center-ish.
        let y = size.height / 2;
        let area = Rect {
            x: size.x,
            y: size.y + y.saturating_sub(1).min(size.height.saturating_sub(1)),
            width: size.width,
            height: size.height.saturating_sub(y).clamp(1, 3),
        };
        f.render_widget(p, area);
        return Ok(());
    }

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Title bar (incl. bold TX-policy banner)
            Constraint::Min(1),    // Main content
            Constraint::Length(1), // TX queue / now-sending strip
            Constraint::Length(3), // Status bar
        ])
        .split(size);

    // Render title bar
    render_title_bar(f, chunks[0], app);

    // Main content layout: a FULL-WIDTH active-QSO banner and waterfall across
    // the top (a wider waterfall = finer horizontal frequency resolution,
    // ~25 Hz/col on a typical terminal), then a two-column lower region. The
    // bottom row of each column lines up — QSO Status (left) sits directly
    // across from Callers (right) — with DX Hunter moved up above Callers to
    // make room for the wide waterfall.
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Percentage(30), // Waterfall (full width)
            Constraint::Min(1),         // Lower region (two columns)
        ])
        .split(chunks[1]);

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(content[2]);

    // Left column: Band Activity (top), QSO Status (bottom).
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(60), // Band activity
            Constraint::Percentage(40), // QSO status
        ])
        .split(lower[0]);

    // Right column: Station Info (small) on top, DX Hunter (moved up) in the
    // middle, Callers on the bottom — aligned with QSO Status across the gutter.
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20), // Station Info
            Constraint::Percentage(40), // DX Hunter
            Constraint::Percentage(40), // Callers (bottom; across from QSO Status)
        ])
        .split(lower[1]);

    // Render panels
    render_active_qsos(f, content[0], app);
    render_waterfall(f, content[1], app);
    render_band_activity(f, left_chunks[0], app)?;
    render_qso_status(f, left_chunks[1], app)?;
    render_station_info(f, right_chunks[0], app)?;
    render_dx_hunter(f, right_chunks[1], app)?;
    render_callers(f, right_chunks[2], app)?;

    // Render status bar
    // TX queue / now-sending strip (between content and status bar).
    render_tx_strip(f, chunks[2], app);

    render_status_bar(f, chunks[3], app);

    // Render active panel highlight. The slice order MUST match the
    // ActivePanel enum order used by render_active_panel_highlight:
    // BandActivity, QsoStatus, StationInfo, Callers, DxHunter. The full-width
    // banner (content[0]) and waterfall (content[1]) are not navigable panels.
    render_active_panel_highlight(
        f,
        app,
        &[
            left_chunks[0],  // BandActivity
            left_chunks[1],  // QsoStatus
            right_chunks[0], // StationInfo
            right_chunks[2], // Callers
            right_chunks[1], // DxHunter
        ],
    );

    Ok(())
}

fn render_title_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let utc_clock = chrono::Utc::now().format("%H:%M:%S UTC").to_string();

    let mut left_spans = vec![
        Span::styled(
            "Pancetta TUI",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.call_sign,
            Style::default()
                .fg(app.theme.success_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.grid_square,
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("{:.3} MHz", app.station_info.operating_frequency),
            Style::default().fg(app.theme.warning_color()),
        ),
        Span::raw(" "),
        Span::styled(
            app.config
                .get_current_band(app.station_info.operating_frequency)
                .map(|b| b.name.as_str())
                .unwrap_or(""),
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.mode,
            Style::default().fg(app.theme.accent_color()),
        ),
    ];

    // Bold, color-coded global TX-policy banner chip. Always visible so
    // the operator can tell at a glance which of the three states is
    // active: GREEN "TX: FULL", YELLOW "TX: RESPOND-ONLY", RED
    // "TX: DISABLED — RX ONLY". Reversed/bold for visual dominance.
    let (policy_text, policy_bg) = match app.tx_policy {
        pancetta_core::TxPolicy::Full => (" TX: FULL ".to_string(), Color::Green),
        pancetta_core::TxPolicy::RespondOnly => (" TX: RESPOND-ONLY ".to_string(), Color::Yellow),
        pancetta_core::TxPolicy::Disabled => (" TX: DISABLED — RX ONLY ".to_string(), Color::Red),
    };
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        policy_text,
        Style::default()
            .fg(Color::Black)
            .bg(policy_bg)
            .add_modifier(Modifier::BOLD),
    ));

    // TX-frequency mode chip: HOLD (operator's offset is sticky) vs AUTO
    // (pancetta picks/adjusts). Cyan = HOLD (locked), Magenta = AUTO (free).
    let (freq_text, freq_bg) = match app.tx_freq_mode {
        pancetta_core::TxFreqMode::Hold => (" FREQ: HOLD ".to_string(), Color::Cyan),
        pancetta_core::TxFreqMode::Auto => (" FREQ: AUTO ".to_string(), Color::Magenta),
    };
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        freq_text,
        Style::default()
            .fg(Color::Black)
            .bg(freq_bg)
            .add_modifier(Modifier::BOLD),
    ));

    // Split-TX chip: shown when the rig is operating split (TX ≠ RX dial).
    if app.split_tx_hz != 0 {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" SPLIT TX {:.3} ", app.split_tx_hz as f64 / 1e6),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // TX audio offset chip: shown when the operator has set a held offset.
    // "TX off: NNNN (HOLD)" when set; hidden when Auto (no noise in the bar).
    if let Some(offset_hz) = app.tx_offset_hold_hz {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" TX off: {} (HOLD) ", offset_hz),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Health alarm chip — prominent, always-visible warning for the highest-
    // stakes silent failures so a new operator (or one whose audio device was
    // hijacked by a remote-desktop client) sees *why* nothing is decoding,
    // rather than just an empty waterfall. Driven by the existing pipeline
    // health snapshot; the bottom status bar still shows the per-stage detail.
    if let Some(ref h) = app.pipeline_health {
        let alarm = if !h.audio_alive {
            Some(" ⚠ AUDIO DEAD — press d ")
        } else if !h.ft8lib_available {
            Some(" ⚠ DECODER STUB ")
        } else {
            None
        };
        if let Some(text) = alarm {
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                text,
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // FCC §97.221 presence prompt: autonomous on, but operator idle → initiation
    // (CQ/pounce) is suppressed until they prove presence with a keypress.
    if app.autonomous_init_paused {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            " ⏸ AUTO-CQ PAUSED — press a key (FCC §97.221) ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // TX indicator
    if app.is_transmitting {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            " TX ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Calculate padding to right-align the UTC clock
    let left_len: usize = left_spans.iter().map(|s| s.width()).sum();
    let clock_len = utc_clock.len();
    let padding = (area.width as usize).saturating_sub(left_len + clock_len);

    left_spans.push(Span::raw(" ".repeat(padding)));
    left_spans.push(Span::styled(
        utc_clock,
        Style::default()
            .fg(app.theme.foreground_color())
            .add_modifier(Modifier::BOLD),
    ));

    let title = Line::from(left_spans);

    let paragraph = Paragraph::new(title).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );

    f.render_widget(paragraph, area);
}

/// One-row TX strip showing what's transmitting RIGHT NOW and what's
/// queued for an upcoming slot. Lightweight: reuses the coordinator's
/// `TxQueueUpdate` snapshot already in `App`.
fn render_tx_strip(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::new();

    // Non-deferred queued items that share the on-air slot are CONCURRENT
    // multi-TX streams (all keyed in the same 15s slot at different audio
    // frequencies), not a future-slot queue. Deferred items are genuinely
    // waiting for a later slot.
    let concurrent: Vec<&crate::app::TxQueueItem> = if app.tx_now_sending.is_some() {
        app.tx_queued.iter().filter(|it| !it.deferred).collect()
    } else {
        Vec::new()
    };
    let deferred: Vec<&crate::app::TxQueueItem> =
        app.tx_queued.iter().filter(|it| it.deferred).collect();

    match &app.tx_now_sending {
        Some(item) => {
            // LIVE TX — make it unmistakable. A bold red chip + bold frame
            // text dominate the strip for the full ~12.64s the message is
            // keyed (set at PTT-assert, cleared at PTT-release). The operator
            // repeatedly reported "I can't see what we're actually
            // transmitting WHILE we're transmitting it" — this is the fix.
            let now_count = 1 + concurrent.len();
            let label = if now_count > 1 {
                format!(" 🔴 TX NOW ×{} ", now_count)
            } else {
                " 🔴 TX NOW ".to_string()
            };
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
            let mut on_air: Vec<String> = vec![format!("{} @{:.0}Hz", item.text, item.freq_hz)];
            on_air.extend(
                concurrent
                    .iter()
                    .map(|it| format!("{} @{:.0}Hz", it.text, it.freq_hz)),
            );
            // The frame text itself, also white-on-red and bold so it reads as
            // a single dominant TX banner rather than a thin easily-missed
            // strip.
            spans.push(Span::styled(
                format!(" {} ", on_air.join("  |  ")),
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        None => {
            spans.push(Span::styled(
                "▶ NOW: (idle) ",
                Style::default().fg(app.theme.foreground_color()),
            ));
        }
    }

    spans.push(Span::raw("  "));

    if deferred.is_empty() {
        spans.push(Span::styled(
            "⋯ QUEUED: (none)",
            Style::default().fg(app.theme.foreground_color()),
        ));
    } else {
        spans.push(Span::styled(
            format!("⋯ QUEUED ({}): ", deferred.len()),
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD),
        ));
        // Show up to the first three deferred items so the strip stays 1 row.
        let shown: Vec<String> = deferred
            .iter()
            .take(3)
            .map(|it| format!("{} @{:.0}Hz → deferred 30s", it.text, it.freq_hz))
            .collect();
        let mut text = shown.join(" | ");
        if deferred.len() > 3 {
            text.push_str(&format!(" | +{} more", deferred.len() - 3));
        }
        spans.push(Span::styled(
            text,
            Style::default().fg(app.theme.warning_color()),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );
    f.render_widget(paragraph, area);
}

fn render_status_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let messages_count = app.decoded_messages.len();
    let dx_count = app.dx_stations.len();

    // Pipeline health indicators
    let (audio_indicator, dsp_indicator, decoder_indicator) = match &app.pipeline_health {
        Some(health) => {
            let audio = if health.audio_alive {
                Span::styled(
                    "AUD",
                    Style::default()
                        .fg(app.theme.success_color())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    "AUD",
                    Style::default()
                        .fg(app.theme.error_color())
                        .add_modifier(Modifier::BOLD),
                )
            };
            let dsp = if health.dsp_windows > 0 {
                Span::styled(
                    format!("DSP:{}", health.dsp_windows),
                    Style::default().fg(app.theme.success_color()),
                )
            } else {
                Span::styled("DSP:0", Style::default().fg(app.theme.error_color()))
            };
            let dec_label = if health.ft8lib_available {
                "FT8"
            } else {
                "FT8(native)"
            };
            let decoder = if health.total_decodes > 0 {
                Span::styled(
                    format!("{}:{}", dec_label, health.total_decodes),
                    Style::default().fg(app.theme.success_color()),
                )
            } else {
                Span::styled(
                    format!("{}:0", dec_label),
                    Style::default().fg(app.theme.warning_color()),
                )
            };
            (audio, dsp, decoder)
        }
        None => (
            Span::styled("AUD", Style::default().fg(app.theme.muted_color())),
            Span::styled("DSP", Style::default().fg(app.theme.muted_color())),
            Span::styled("FT8", Style::default().fg(app.theme.muted_color())),
        ),
    };

    let mut status_spans = vec![
        audio_indicator,
        Span::raw(" "),
        dsp_indicator,
        Span::raw(" "),
        decoder_indicator,
        Span::raw(" | "),
        Span::styled(
            format!("Level: {:.1}%", app.audio_level * 100.0),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("Msgs: {}", messages_count),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("DX: {}", dx_count),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        // While composing a free-text TX message, show the live input line
        // (with cursor) in bold instead of the transient status text so the
        // operator sees exactly what they're about to send.
        match app.compose_prompt() {
            Some(prompt) => Span::styled(
                prompt,
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ),
            None => Span::styled(
                app.status_message.clone(),
                Style::default().fg(app.theme.accent_color()),
            ),
        },
    ];

    // SWR — shown only while transmitting (swr_display() returns Some only for
    // a fresh reading, which is sampled solely during TX). Prepend it, bold and
    // color-graded by match quality, so it's unmistakable when keyed.
    if let Some(swr_text) = app.swr_display() {
        let color = match app.swr {
            Some(s) if s >= 3.0 => app.theme.error_color(),
            Some(s) if s >= 2.0 => app.theme.warning_color(),
            _ => app.theme.success_color(),
        };
        status_spans.insert(0, Span::raw(" | "));
        status_spans.insert(
            0,
            Span::styled(
                swr_text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        );
    }

    let status_line = Line::from(status_spans);

    // Always-visible key hints. These MUST match the real bindings in
    // tui_runner.rs (the help-overlay is the only other on-screen keymap):
    // band is `=`/`-` (not `+`), quit is `q` (not Ctrl+Q), `?` opens full help,
    // `d` opens the audio-device picker (the common "reclaim my output" action).
    let key_hint = |k: &'static str| {
        Span::styled(
            k,
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        )
    };
    let help_line = Line::from(vec![
        key_hint("Space"),
        Span::raw(":Call/Reply | "),
        key_hint("Arrows"),
        Span::raw(":TX | "),
        key_hint("=/-"),
        Span::raw(":Band | "),
        key_hint("g"),
        Span::raw(":TX-policy | "),
        key_hint("d"),
        Span::raw(":Audio | "),
        key_hint("?"),
        Span::raw(":Help | "),
        key_hint("q"),
        Span::raw(":Quit"),
    ]);

    // Split status bar into two lines
    let status_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let status_paragraph = Paragraph::new(status_line).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );

    let help_paragraph = Paragraph::new(help_line).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.muted_color()),
    );

    f.render_widget(status_paragraph, status_chunks[0]);
    f.render_widget(help_paragraph, status_chunks[1]);

    // Border line
    let border_line = Line::from(vec![Span::raw("─".repeat(area.width as usize))]);
    let border_paragraph =
        Paragraph::new(border_line).style(Style::default().fg(app.theme.border_color()));
    f.render_widget(border_paragraph, status_chunks[2]);
}

fn render_waterfall(f: &mut Frame<'_>, area: Rect, app: &App) {
    // Collect recent decoded signal frequencies
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(30);
    let signal_freqs: Vec<f64> = app
        .decoded_messages
        .iter()
        .filter(|m| m.timestamp > cutoff)
        .map(|m| m.delta_freq as f64)
        .collect();

    // Build (freq, parity, timestamp) tuples for the occupancy strip from
    // recent decodes. Filter to last 60s; the widget further trims per-column
    // (±37.5 Hz of column center, and re-checks the 60s cutoff defensively).
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(60);
    let decoded_for_occupancy: Vec<(
        f64,
        pancetta_core::slot::SlotParity,
        chrono::DateTime<chrono::Utc>,
    )> = app
        .decoded_messages
        .iter()
        .filter(|m| m.timestamp >= cutoff)
        .filter_map(|m| m.slot_parity.map(|p| (m.delta_freq as f64, p, m.timestamp)))
        .collect();
    let tx_parity = app.resolve_tx_parity();

    // When something is actually on the air (autonomous/QSO TX), the green TX
    // cursor and its label should follow the LIVE TX frequency, not the manual
    // waterfall offset (the 1350→2300 visual bug). Fall back to the operator's
    // manual offset only when idle.
    let (cursor_offset, title) = match &app.tx_now_sending {
        Some(item) => (
            item.freq_hz,
            format!(" Waterfall [/]: TX {:.0} Hz (LIVE) ", item.freq_hz),
        ),
        None => (
            app.tx_frequency_offset,
            format!(" Waterfall [/]: TX {:.0} Hz ", app.tx_frequency_offset),
        ),
    };
    let waterfall_block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(app.theme.accent_color()),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border_color()));

    let waterfall = Waterfall::new(&app.waterfall_data)
        .block(waterfall_block)
        .tx_offset(cursor_offset)
        .signal_freqs(signal_freqs)
        .color_capability(app.color_capability)
        .decoded_for_occupancy(&decoded_for_occupancy)
        .tx_parity(tx_parity);
    f.render_widget(waterfall, area);
}

fn render_active_panel_highlight(f: &mut Frame<'_>, app: &App, panel_areas: &[Rect]) {
    let active_area = match app.active_panel {
        ActivePanel::BandActivity => panel_areas[0],
        ActivePanel::QsoStatus => panel_areas[1],
        ActivePanel::StationInfo => panel_areas[2],
        ActivePanel::Callers => panel_areas[3],
        ActivePanel::DxHunter => panel_areas[4],
    };

    // Draw a subtle highlight border around the active panel
    let highlight_block = Block::default().borders(Borders::ALL).border_style(
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD),
    );

    f.render_widget(highlight_block, active_area);
}

/// Create a styled block for panels
pub fn create_panel_block<'a>(title: &'a str, is_active: bool, app: &App) -> Block<'a> {
    let border_style = if is_active {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.border_color())
    };

    let title_style = if is_active {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.accent_color())
    };

    Block::default()
        .title(Span::styled(title, title_style))
        .borders(Borders::ALL)
        .border_style(border_style)
}

/// Render the Shift+F frequency-entry modal.
pub fn render_freq_modal(f: &mut Frame<'_>, area: Rect, m: &crate::app::FreqModalState) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 52.min(area.width.saturating_sub(4));
    let modal_height: u16 = 8.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let rx_focus = matches!(m.field, crate::app::FreqModalField::RxDial);
    let body = format!(
        " RX dial (MHz): {}{}\n TX split (MHz): {}{}\n   (blank = simplex)\n\n [Enter] apply   [Tab] field   [Esc] cancel",
        m.rx_buffer,
        if rx_focus { "_" } else { "" },
        m.tx_buffer,
        if !rx_focus { "_" } else { "" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Set Frequency ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Render the `o`-key TX-audio-offset modal.
///
/// One integer Hz field in [300, 2700]; blank Enter = Auto; Esc = cancel.
/// Mirrors the `render_freq_modal` layout and sizing.
pub fn render_offset_modal(f: &mut Frame<'_>, area: Rect, m: &crate::app::OffsetModalState) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 52.min(area.width.saturating_sub(4));
    let modal_height: u16 = 6.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let body = format!(
        " TX audio offset (Hz, 300–2700): {}_\n   blank = Auto (Tx=Rx)\n\n [Enter] apply   [Esc] cancel",
        m.buffer,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Set TX Offset ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Render the required out-of-band acknowledgment modal.
pub fn render_out_of_band_modal(f: &mut Frame<'_>, area: Rect, tx_rf_hz: u64) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 60.min(area.width.saturating_sub(4));
    let modal_height: u16 = 7.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let body = format!(
        " TX {:.3} MHz is OUTSIDE the US ham bands.\n You are responsible for legal operation.\n\n [Enter] acknowledge",
        tx_rf_hz as f64 / 1e6,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" \u{26a0} Out of band ")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Helper function to get panel-specific colors
pub fn get_snr_color(snr: i32, theme: &crate::config::Theme) -> Color {
    match snr {
        snr if snr >= 0 => theme.success_color(),
        snr if snr >= -10 => theme.warning_color(),
        _ => theme.error_color(),
    }
}

/// Helper function to format distance
pub fn format_distance(distance: Option<f64>) -> String {
    match distance {
        Some(d) if d < 1000.0 => format!("{:.0} km", d),
        Some(d) => format!("{:.1}k km", d / 1000.0),
        None => "---".to_string(),
    }
}

/// Helper function to format bearing
pub fn format_bearing(bearing: Option<f64>) -> String {
    match bearing {
        Some(b) => format!("{:.0}°", b),
        None => "---".to_string(),
    }
}

/// Helper function to format time ago
pub fn format_time_ago(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration.num_seconds() < 60 {
        format!("{}s", duration.num_seconds())
    } else if duration.num_minutes() < 60 {
        format!("{}m", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h", duration.num_hours())
    } else {
        format!("{}d", duration.num_days())
    }
}
