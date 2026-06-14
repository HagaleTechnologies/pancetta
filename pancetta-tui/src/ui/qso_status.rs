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
                Constraint::Length(3), // Sequence ladder + Now/Next
                Constraint::Length(2), // TX/RX status
                Constraint::Length(2), // SNR meters
                Constraint::Min(1),    // Progress/timing
                Constraint::Length(1), // Control hint
            ])
            .split(block.inner(area));

        f.render_widget(block, area);

        render_qso_info(f, chunks[0], app);
        render_ladder(f, chunks[1], app);
        render_tx_rx_status(f, chunks[2], app);
        render_snr_meters(f, chunks[3], app);
        render_timing_progress(f, chunks[4], app);
        render_control_hint(f, chunks[5], app);
    }

    Ok(())
}

fn render_multi_qso_table(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();

    // Header
    lines.push(Line::from(vec![Span::styled(
        "  Call       Freq     State         Step    SNR  Exch ",
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    )]));

    // Each active QSO. `cursor` selects which row is highlighted; it indexes
    // into the stored `active_qsos`/`qso_statuses` order (same order the
    // selection cursor uses), so the highlight and the abort/re-send target
    // always agree.
    let cursor = app.qso_cursor.min(app.active_qsos.len().saturating_sub(1));
    for (idx, qso) in app.qso_statuses.iter().enumerate() {
        if !qso.active {
            continue;
        }
        let selected = idx == cursor;
        let marker = if selected { "▶ " } else { "  " };
        let call = qso.call_sign.as_deref().unwrap_or("---");
        let freq = qso
            .frequency
            .map_or("---".to_string(), |f| format!("{:.0}", f));
        let state = qso.state.as_deref().unwrap_or("---");
        // Current rung label from the ladder, if any.
        let step = qso
            .ladder_labels
            .get(qso.ladder_index)
            .map(|s| s.as_str())
            .unwrap_or("-");
        let snr = qso.snr_rx.map_or("---".to_string(), |s| format!("{:+}", s));

        let base_fg = if selected {
            app.theme.accent_color()
        } else {
            app.theme.foreground_color()
        };
        let mut call_style = Style::default().fg(base_fg).add_modifier(Modifier::BOLD);
        if selected {
            call_style = call_style.add_modifier(Modifier::REVERSED);
        }

        lines.push(Line::from(vec![
            Span::styled(format!("{}{:<10}", marker, call), call_style),
            Span::styled(
                format!("{:>7}  ", freq),
                Style::default().fg(app.theme.warning_color()),
            ),
            Span::styled(
                format!("{:<13}", state),
                Style::default().fg(app.theme.accent_color()),
            ),
            Span::styled(
                format!("{:<7} ", step),
                Style::default().fg(app.theme.warning_color()),
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

    // Control hint for the multi-QSO view.
    lines.push(Line::from(Span::styled(
        " [k] abort  [r] re-send  Up/Down select",
        Style::default().fg(app.theme.muted_color()),
    )));

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
        .map_or("---".to_string(), |f| format!("{:.0} Hz", f));
    let mode_text = qso.mode.as_deref().unwrap_or("---");
    let state_text = qso.state.as_deref().unwrap_or("---");
    let sent_text = qso
        .report_sent
        .map_or("---".to_string(), |r| format!("{:+}", r));
    let rcvd_text = qso
        .report_received
        .map_or("---".to_string(), |r| format!("{:+}", r));

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
        // Batch 94: state-machine phase + reports exchanged. The QSO
        // engine pushes these live via ActiveQsosUpdate snapshots.
        Line::from(vec![
            Span::styled("State: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                state_text,
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Sent: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(sent_text, Style::default().fg(app.theme.warning_color())),
            Span::raw("  "),
            Span::styled("Rcvd: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(rcvd_text, Style::default().fg(app.theme.success_color())),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// Render the role-aware sequence ladder plus the Now/Next lines for the
/// selected (single-detail) QSO. The ladder shows the canonical exchange
/// as a row of rungs joined by " ── "; rungs we transmit are prefixed with
/// a small marker, the current rung is wrapped in [ ] and highlighted.
fn render_ladder(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // No ladder available (terminal/idle/contest, or no QSO yet): render
    // a quiet placeholder so the layout stays stable.
    if !qso.active || qso.ladder_labels.is_empty() {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "Seq : ---",
            Style::default().fg(app.theme.muted_color()),
        )));
        f.render_widget(paragraph, area);
        return;
    }

    let mut spans: Vec<Span> = vec![Span::styled(
        "Seq : ",
        Style::default().fg(app.theme.foreground_color()),
    )];

    for (i, label) in qso.ladder_labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " ── ",
                Style::default().fg(app.theme.muted_color()),
            ));
        }
        let ours = qso.ladder_ours.get(i).copied().unwrap_or(false);
        let prefix = if ours { "·" } else { "" };
        let is_current = i == qso.ladder_index;
        let text = if is_current {
            format!("[{}{}]", prefix, label)
        } else {
            format!("{}{}", prefix, label)
        };
        let style = if is_current {
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD)
        } else if ours {
            Style::default().fg(app.theme.accent_color())
        } else {
            Style::default().fg(app.theme.foreground_color())
        };
        spans.push(Span::styled(text, style));
    }

    let now = if qso.now_line.is_empty() {
        "---".to_string()
    } else {
        qso.now_line.clone()
    };
    let next = if qso.next_line.is_empty() {
        "---".to_string()
    } else {
        qso.next_line.clone()
    };

    let lines = vec![
        Line::from(spans),
        Line::from(vec![
            Span::styled("Now : ", Style::default().fg(app.theme.muted_color())),
            Span::styled(now, Style::default().fg(app.theme.foreground_color())),
            Span::styled("   Next: ", Style::default().fg(app.theme.muted_color())),
            Span::styled(next, Style::default().fg(app.theme.muted_color())),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// One-line control hint at the bottom of the single-detail QSO panel.
fn render_control_hint(f: &mut Frame<'_>, area: Rect, app: &App) {
    let paragraph = Paragraph::new(Line::from(Span::styled(
        "[k] abort  [r] re-send  Up/Down select",
        Style::default().fg(app.theme.muted_color()),
    )));
    f.render_widget(paragraph, area);
}

fn render_tx_rx_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // Batch 94: show the last message exchanged in each direction, with
    // a time-ago suffix when the timestamp is known.
    let tx_status = format_direction_line("TX", qso.last_tx_text.as_deref(), qso.last_tx);
    let rx_status = format_direction_line("RX", qso.last_rx_text.as_deref(), qso.last_rx);

    let lines = vec![
        Line::from(Span::styled(
            tx_status,
            Style::default().fg(app.theme.warning_color()),
        )),
        Line::from(Span::styled(
            rx_status,
            Style::default().fg(app.theme.success_color()),
        )),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// Build a "TX: JA1ABC K5ARH EM10 (12s ago)" style line for one
/// direction of the last-message display. Falls back gracefully when
/// only the timestamp or neither is known.
fn format_direction_line(
    label: &str,
    text: Option<&str>,
    at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    match (text, at) {
        (Some(text), Some(at)) => format!("{}: {} ({})", label, text, format_time_ago(at)),
        (Some(text), None) => format!("{}: {}", label, text),
        (None, Some(at)) => format!("{}: {}", label, format_time_ago(at)),
        (None, None) => format!("{}: Never", label),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snr_gauge_color() {
        use crate::config::Theme;
        let theme = Theme::Dark;

        assert_eq!(get_snr_gauge_color(15, &theme), theme.success_color());
        assert_eq!(get_snr_gauge_color(5, &theme), theme.warning_color());
        assert_eq!(get_snr_gauge_color(-15, &theme), theme.error_color());
    }

    /// Batch 94: the TX/RX line shows the last message text plus a
    /// time-ago suffix, degrading gracefully when either is missing.
    #[test]
    fn test_format_direction_line() {
        let at = chrono::Utc::now() - chrono::Duration::seconds(12);
        let line = format_direction_line("TX", Some("JA1ABC K5ARH EM10"), Some(at));
        assert!(line.starts_with("TX: JA1ABC K5ARH EM10 ("));
        assert!(line.ends_with(')'));

        assert_eq!(
            format_direction_line("RX", Some("K5ARH JA1ABC -12"), None),
            "RX: K5ARH JA1ABC -12"
        );
        assert_eq!(format_direction_line("RX", None, None), "RX: Never");
        let only_time = format_direction_line("TX", None, Some(at));
        assert!(only_time.starts_with("TX: "));
        assert_ne!(only_time, "TX: Never");
    }
}
