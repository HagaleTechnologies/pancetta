use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Gauge, Paragraph},
    Frame,
};

use super::{create_panel_block, format_time_ago};
use crate::app::{ActivePanel, App};

pub fn render_qso_status(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::QsoStatus);
    let active_count = app.qso_statuses.iter().filter(|q| q.active).count();

    let title = if active_count > 1 {
        format!("QSO Status ({}/{})", active_count, app.qso_statuses.len())
    } else {
        "QSO Status".to_string()
    };
    let block = create_panel_block(&title, is_active, app);

    if active_count > 1 {
        // Multi-QSO table view
        let inner = block.inner(area);
        f.render_widget(block, area);
        render_multi_qso_table(f, inner, app);
    } else {
        // Single QSO detail view (original layout)
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // QSO info
                Constraint::Length(2), // TX/RX status
                Constraint::Length(2), // SNR meters
                Constraint::Min(1),    // Progress/timing
            ])
            .split(block.inner(area));

        f.render_widget(block, area);

        render_qso_info(f, chunks[0], app);
        render_tx_rx_status(f, chunks[1], app);
        render_snr_meters(f, chunks[2], app);
        render_timing_progress(f, chunks[3], app);
    }

    Ok(())
}

fn render_multi_qso_table(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(
            " Call       Freq     Mode  SNR  Exch ",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    // Each active QSO
    for qso in &app.qso_statuses {
        if !qso.active {
            continue;
        }
        let call = qso.call_sign.as_deref().unwrap_or("---");
        let freq = qso
            .frequency
            .map_or("---".to_string(), |f| format!("{:.0}", f));
        let mode = qso.mode.as_deref().unwrap_or("FT8");
        let snr = qso.snr_rx.map_or("---".to_string(), |s| format!("{:+}", s));

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<10}", call),
                Style::default()
                    .fg(app.theme.foreground_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>7}  ", freq),
                Style::default().fg(app.theme.warning_color()),
            ),
            Span::styled(
                format!("{:<5}", mode),
                Style::default().fg(app.theme.accent_color()),
            ),
            Span::styled(
                format!("{:>4}  ", snr),
                Style::default().fg(app.theme.success_color()),
            ),
            Span::styled(
                format!("{:>3}", qso.exchange_count),
                Style::default().fg(app.theme.foreground_color()),
            ),
        ]));
    }

    if lines.len() == 1 {
        lines.push(Line::from(Span::styled(
            " No active QSOs",
            Style::default().fg(app.theme.muted_color()),
        )));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_qso_info(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    let status_text = if qso.active { "ACTIVE QSO" } else { "STANDBY" };

    let status_style = if qso.active {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted_color())
    };

    let call_text = qso.call_sign.as_deref().unwrap_or("---");
    let freq_text = qso
        .frequency
        .map_or("---".to_string(), |f| format!("{:.3}", f));
    let mode_text = qso.mode.as_deref().unwrap_or("---");

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "Status: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(status_text, status_style),
            Span::raw("  "),
            Span::styled("Call: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                call_text,
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Freq: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(freq_text, Style::default().fg(app.theme.warning_color())),
            Span::raw("  "),
            Span::styled("Mode: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(mode_text, Style::default().fg(app.theme.accent_color())),
            Span::raw("  "),
            Span::styled(
                "Exchanges: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                format!("{}", qso.exchange_count),
                Style::default().fg(app.theme.success_color()),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_tx_rx_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    let tx_status = if let Some(last_tx) = qso.last_tx {
        format!("TX: {}", format_time_ago(last_tx))
    } else {
        "TX: Never".to_string()
    };

    let rx_status = if let Some(last_rx) = qso.last_rx {
        format!("RX: {}", format_time_ago(last_rx))
    } else {
        "RX: Never".to_string()
    };

    let lines = vec![Line::from(vec![
        Span::styled(tx_status, Style::default().fg(app.theme.warning_color())),
        Span::raw("    "),
        Span::styled(rx_status, Style::default().fg(app.theme.success_color())),
    ])];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_snr_meters(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // Split into two columns for TX and RX SNR
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // TX SNR Gauge
    let tx_snr = qso.snr_tx.unwrap_or(-50);
    let tx_ratio = ((tx_snr + 50) as f64 / 80.0).clamp(0.0, 1.0); // Map -50 to +30 dB to 0-1

    let tx_gauge = Gauge::default()
        .block(ratatui::widgets::Block::default().title("TX SNR"))
        .gauge_style(Style::default().fg(get_snr_gauge_color(tx_snr, &app.theme)))
        .ratio(tx_ratio)
        .label(format!("{:+} dB", tx_snr));

    f.render_widget(tx_gauge, chunks[0]);

    // RX SNR Gauge
    let rx_snr = qso.snr_rx.unwrap_or(-50);
    let rx_ratio = ((rx_snr + 50) as f64 / 80.0).clamp(0.0, 1.0);

    let rx_gauge = Gauge::default()
        .block(ratatui::widgets::Block::default().title("RX SNR"))
        .gauge_style(Style::default().fg(get_snr_gauge_color(rx_snr, &app.theme)))
        .ratio(rx_ratio)
        .label(format!("{:+} dB", rx_snr));

    f.render_widget(rx_gauge, chunks[1]);
}

fn render_timing_progress(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    if !qso.active {
        // Show general monitoring status
        let lines = vec![
            Line::from(vec![Span::styled(
                "Monitoring for calls...",
                Style::default().fg(app.theme.muted_color()),
            )]),
            Line::from(vec![
                Span::styled(
                    "Audio Level: ",
                    Style::default().fg(app.theme.foreground_color()),
                ),
                Span::styled(
                    format!("{:.1}%", app.audio_level * 100.0),
                    Style::default().fg(get_audio_level_color(app.audio_level, &app.theme)),
                ),
            ]),
        ];

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, area);
        return;
    }

    // Show QSO timing and progress
    let duration = if let Some(started) = qso.started_at {
        let elapsed = chrono::Utc::now().signed_duration_since(started);
        format!(
            "Duration: {}m {}s",
            elapsed.num_minutes(),
            elapsed.num_seconds() % 60
        )
    } else {
        "Duration: Unknown".to_string()
    };

    // FT8 cycle timing (15-second cycles)
    let now = chrono::Utc::now();
    let seconds_in_cycle = now.timestamp() % 15;
    let cycle_progress = seconds_in_cycle as f64 / 15.0;

    let cycle_status = if seconds_in_cycle < 13 {
        "RX Period"
    } else {
        "TX Window"
    };

    let lines = vec![Line::from(vec![
        Span::styled(duration, Style::default().fg(app.theme.foreground_color())),
        Span::raw("  "),
        Span::styled(cycle_status, Style::default().fg(app.theme.accent_color())),
    ])];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);

    // Render cycle progress bar if area is tall enough
    if area.height > 2 {
        let progress_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };

        let cycle_gauge = Gauge::default()
            .ratio(cycle_progress)
            .gauge_style(Style::default().fg(app.theme.accent_color()))
            .label(format!("{}s", 15 - seconds_in_cycle));

        f.render_widget(cycle_gauge, progress_area);
    }
}

fn get_snr_gauge_color(snr: i32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match snr {
        snr if snr >= 10 => theme.success_color(),
        snr if snr >= 0 => theme.warning_color(),
        snr if snr >= -10 => theme.warning_color(),
        _ => theme.error_color(),
    }
}

fn get_audio_level_color(level: f32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match level {
        level if level > 0.8 => theme.error_color(), // Too loud
        level if level > 0.1 => theme.success_color(), // Good level
        level if level > 0.01 => theme.warning_color(), // Low but present
        _ => theme.muted_color(),                    // Very low/no signal
    }
}

/// Update QSO status based on new message
pub fn update_qso_from_message(app: &mut App, message: &crate::app::DecodedMessage) {
    let our_call = &app.station_info.call_sign.to_uppercase();
    let message_upper = message.message.to_uppercase();

    // Check if this message involves our station
    if message_upper.contains(our_call) {
        // Extract the other station's call sign
        if let Some(other_call) = extract_other_callsign(&message_upper, our_call) {
            let qso = app.qso_status_mut();

            if !qso.active {
                // Start new QSO
                qso.active = true;
                qso.call_sign = Some(other_call);
                qso.frequency = Some(message.frequency);
                qso.mode = Some(message.mode.clone());
                qso.started_at = Some(message.timestamp);
                qso.exchange_count = 0;
            }

            // Update receive time and SNR
            qso.last_rx = Some(message.timestamp);
            qso.snr_rx = Some(message.snr);
            qso.exchange_count += 1;
        }
    }
}

fn extract_other_callsign(message: &str, our_call: &str) -> Option<String> {
    let parts: Vec<&str> = message.split_whitespace().collect();

    for part in parts {
        if part != our_call && is_valid_callsign(part) {
            return Some(part.to_string());
        }
    }

    None
}

fn is_valid_callsign(s: &str) -> bool {
    // Basic callsign validation
    s.len() >= 3
        && s.len() <= 10
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_other_callsign() {
        assert_eq!(
            extract_other_callsign("K1ABC W2XYZ RRR", "K1ABC"),
            Some("W2XYZ".to_string())
        );
        assert_eq!(
            extract_other_callsign("W2XYZ K1ABC 73", "K1ABC"),
            Some("W2XYZ".to_string())
        );
        assert_eq!(
            extract_other_callsign("CQ DX K1ABC FN42", "W2XYZ"),
            Some("K1ABC".to_string())
        );
    }

    #[test]
    fn test_snr_gauge_color() {
        use crate::config::Theme;
        let theme = Theme::Dark;

        assert_eq!(get_snr_gauge_color(15, &theme), theme.success_color());
        assert_eq!(get_snr_gauge_color(5, &theme), theme.warning_color());
        assert_eq!(get_snr_gauge_color(-15, &theme), theme.error_color());
    }
}
