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

pub mod band_activity;
pub mod dx_hunter;
pub mod qso_status;
pub mod station_info;

use band_activity::render_band_activity;
use dx_hunter::render_dx_hunter;
use qso_status::render_qso_status;
use station_info::render_station_info;

/// Main UI rendering function
pub fn draw(f: &mut Frame<'_>, app: &App) -> Result<()> {
    let size = f.area();

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Title bar
            Constraint::Min(1),    // Main content
            Constraint::Length(3), // Status bar
        ])
        .split(size);

    // Render title bar
    render_title_bar(f, chunks[0], app);

    // Create main content layout (2x2 grid)
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[1]);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(45), // Band activity
            Constraint::Percentage(30), // Waterfall
            Constraint::Percentage(25), // QSO status
        ])
        .split(main_chunks[0]);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
        .split(main_chunks[1]);

    // Render panels
    render_band_activity(f, left_chunks[0], app)?;
    render_waterfall(f, left_chunks[1], app);
    render_qso_status(f, left_chunks[2], app)?;
    render_station_info(f, right_chunks[0], app)?;
    render_dx_hunter(f, right_chunks[1], app)?;

    // Render status bar
    render_status_bar(f, chunks[2], app);

    // Render active panel highlight
    // Indices map to ActivePanel enum order: BandActivity, QsoStatus, StationInfo, DxHunter
    // left_chunks[1] (waterfall) is skipped — it's not a navigable panel
    render_active_panel_highlight(
        f,
        app,
        &[
            left_chunks[0],  // BandActivity
            left_chunks[2],  // QsoStatus
            right_chunks[0], // StationInfo
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
                Span::styled(
                    "DSP:0",
                    Style::default().fg(app.theme.error_color()),
                )
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

    let status_line = Line::from(vec![
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
        Span::styled(
            &app.status_message,
            Style::default().fg(app.theme.accent_color()),
        ),
    ]);

    let help_line = Line::from(vec![
        Span::styled(
            "Arrows",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(":TX | "),
        Span::styled(
            "+/-",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(":Band | "),
        Span::styled(
            "Ctrl+Q",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
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

    let title = format!(" Waterfall [/]: TX {:.0} Hz ", app.tx_frequency_offset);
    let waterfall_block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(app.theme.accent_color()),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border_color()));

    let waterfall = Waterfall::new(&app.waterfall_data)
        .block(waterfall_block)
        .tx_offset(app.tx_frequency_offset)
        .signal_freqs(signal_freqs)
        .color_capability(app.color_capability);
    f.render_widget(waterfall, area);
}

fn render_active_panel_highlight(f: &mut Frame<'_>, app: &App, panel_areas: &[Rect]) {
    let active_area = match app.active_panel {
        ActivePanel::BandActivity => panel_areas[0],
        ActivePanel::QsoStatus => panel_areas[1],
        ActivePanel::StationInfo => panel_areas[2],
        ActivePanel::DxHunter => panel_areas[3],
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
