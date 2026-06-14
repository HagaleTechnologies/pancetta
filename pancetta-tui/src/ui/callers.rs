//! "Callers" panel — stations currently calling US.
//!
//! Lists every directed-at-us decode (one row per callsign, newest wins),
//! lets the operator pick one, and shows the reply sequence step pancetta
//! would send — a smart default classified from the caller's last message,
//! overridable with Left/Right. Enter commits the reply (see `tui_runner`).
//! Mirrors the DX-Hunter panel's Table + TableState structure.

use anyhow::Result;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, TableState},
    Frame,
};

use super::{create_panel_block, format_time_ago};
use crate::app::{ActivePanel, App, DecodedMessageView};

pub fn render_callers(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::Callers);
    let block = create_panel_block("Callers", is_active, app);

    let header_cells = ["Call", "Msg", "SNR", "Freq", "Age", "Flags"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1);

    // Same ordering the selection resolver walks.
    let callers: Vec<&DecodedMessageView> = app.displayed_callers();

    // Reserve one row at the bottom for the reply-hint line.
    let table_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let hint_area = Rect {
        x: area.x + 1,
        y: area.y + area.height.saturating_sub(1),
        width: area.width.saturating_sub(2),
        height: 1,
    };

    let mut rows: Vec<Row> = callers.iter().map(|m| caller_row(m, app)).collect();
    if rows.is_empty() {
        rows.push(Row::new(
            [
                Cell::from(""),
                Cell::from("No one calling"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ]
            .into_iter()
            .map(|c| c.style(Style::default().fg(app.theme.muted_color()))),
        ));
    }

    let widths = [
        Constraint::Min(8),     // Call
        Constraint::Min(10),    // Msg
        Constraint::Length(4),  // SNR
        Constraint::Length(5),  // Freq
        Constraint::Length(4),  // Age
        Constraint::Length(12), // Flags
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()))
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut table_state = TableState::default();
    if is_active && !callers.is_empty() {
        let sel = app.callers_scroll.min(callers.len().saturating_sub(1));
        table_state.select(Some(sel));
    }

    f.render_stateful_widget(table, table_area, &mut table_state);

    // Reply-hint line: "reply ◂ R-10 ▸   Enter=send".
    let hint = if callers.is_empty() {
        Line::from(Span::styled(
            "(no callers)",
            Style::default().fg(app.theme.muted_color()),
        ))
    } else {
        let step = app.current_caller_reply_step();
        let label = reply_label(step, app.caller_report_value());
        Line::from(vec![
            Span::styled("reply ", Style::default().fg(app.theme.muted_color())),
            Span::styled("◂ ", Style::default().fg(app.theme.accent_color())),
            Span::styled(
                label,
                Style::default()
                    .fg(app.theme.selected_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▸", Style::default().fg(app.theme.accent_color())),
            Span::styled(
                "   Enter=send",
                Style::default().fg(app.theme.muted_color()),
            ),
        ])
    };
    f.render_widget(Paragraph::new(hint), hint_area);

    Ok(())
}

/// The concrete reply this step would transmit, with the report value filled
/// in where the message carries one.
fn reply_label(step: pancetta_core::ResponseStep, report: i8) -> String {
    use pancetta_core::ResponseStep;
    match step {
        ResponseStep::Grid => "grid".to_string(),
        ResponseStep::Report => format!("{:+}", report),
        ResponseStep::ReportAck => format!("R{:+}", report),
        ResponseStep::Rr73 => "RR73".to_string(),
        ResponseStep::SeventyThree => "73".to_string(),
    }
}

fn caller_row<'a>(msg: &'a DecodedMessageView, app: &App) -> Row<'a> {
    let call = msg.call_sign.clone().unwrap_or_default();

    // Truncate their raw message for the Msg column.
    let mut text = msg.message.clone();
    if text.chars().count() > 16 {
        text = text.chars().take(15).collect::<String>() + "…";
    }

    let snr_str = format!("{:+}", msg.snr);
    let freq_str = format!("{}", (msg.delta_freq.round() as i64).clamp(0, 9999));
    let age_str = format_time_ago(msg.timestamp);

    // Flags: BUSY (mid-exchange with a third party), MINE (already in QSO with
    // us), WKD (worked before on this band).
    let mut flags: Vec<&str> = Vec::new();
    if app.is_caller_busy(&call) {
        flags.push("BUSY");
    }
    if app.is_caller_mine(&call) {
        flags.push("MINE");
    }
    if msg.worked_before {
        flags.push("WKD");
    }
    let flags_str = flags.join(" ");

    let dim = Style::default().fg(app.theme.foreground_color());
    let call_style = Style::default()
        .fg(app.theme.accent_color())
        .add_modifier(Modifier::BOLD);
    let snr_style = Style::default().fg(if msg.snr >= 0 {
        app.theme.success_color()
    } else if msg.snr >= -10 {
        app.theme.warning_color()
    } else {
        app.theme.error_color()
    });
    let flags_style = if flags.contains(&"BUSY") {
        Style::default().fg(app.theme.warning_color())
    } else {
        Style::default().fg(app.theme.muted_color())
    };

    Row::new([
        Cell::from(call).style(call_style),
        Cell::from(text).style(dim),
        Cell::from(snr_str).style(snr_style),
        Cell::from(freq_str).style(Style::default().fg(app.theme.accent_color())),
        Cell::from(age_str).style(Style::default().fg(app.theme.muted_color())),
        Cell::from(flags_str).style(flags_style),
    ])
}
