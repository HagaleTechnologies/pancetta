use anyhow::Result;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Row, Table, TableState},
    Frame,
};

use super::{create_panel_block, format_distance, get_snr_color};
use crate::app::{ActivePanel, App, DecodedMessageView};

pub fn render_band_activity(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::BandActivity);
    let block = create_panel_block("Band Activity", is_active, app);

    // Prepare table data
    //
    // Task 20a: `Freq` and `Mode` are per-row-constant — every row shows the
    // same dial MHz and the same station-wide mode, so the columns carry no
    // per-decode information (unlike SNR/DT/DF/Call/Grid/Dist which vary row
    // to row). Dropped to give `Msg` more room.
    let header_cells = ["Time", "SNR", "DT", "DF", "Call", "Grid", "Dist", "Msg"]
        .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells).height(1).bottom_margin(0);

    // Walk the App's displayed-order iterator: directed-at-us decodes
    // pinned to the top in newest-first order, then everything else in
    // newest-first order. Both this renderer and App::get_selected_station
    // walk the same ordering so the highlighted row matches the selected
    // callsign on Space-press.
    let displayed = app.displayed_messages();
    let mut rows: Vec<Row> = displayed
        .iter()
        .map(|msg| create_message_row(msg, app))
        .collect();

    // If no messages, show placeholder
    if rows.is_empty() {
        rows.push(Row::new(
            [
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from("No messages"),
                Cell::from(""),
                Cell::from(""),
                Cell::from("Monitoring..."),
            ]
            .iter()
            .map(|c| {
                c.clone()
                    .style(Style::default().fg(app.theme.muted_color()))
            }),
        ));
    }

    let widths = [
        Constraint::Length(8),  // Time
        Constraint::Length(4),  // SNR
        Constraint::Length(5),  // DT  (fits "-15.2")
        Constraint::Length(6),  // DF  (fits "+2800")
        Constraint::Length(10), // Call (widened to absorb the engaged "● " prefix)
        Constraint::Length(4),  // Grid
        Constraint::Length(6),  // Dist
        Constraint::Min(20),    // Message (freed space from dropped Freq/Mode columns)
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()))
        .row_highlight_style(
            Style::default()
                .bg(app.theme.accent_color())
                .fg(ratatui::style::Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    // Create table state for potential selection
    let mut table_state = TableState::default();
    if is_active && !app.decoded_messages.is_empty() {
        table_state.select(Some(app.band_activity_scroll));
    }

    f.render_stateful_widget(table, area, &mut table_state);

    // Show scroll indicator if there are more messages
    if app.decoded_messages.len() > (area.height as usize).saturating_sub(4) {
        let scroll_info = format!(
            "{}/{}",
            app.band_activity_scroll + 1,
            app.decoded_messages.len()
        );

        let scroll_area = Rect {
            x: area.x + area.width.saturating_sub(scroll_info.len() as u16 + 2),
            y: area.y,
            width: scroll_info.len() as u16 + 1,
            height: 1,
        };

        let scroll_text = Line::from(Span::styled(
            scroll_info,
            Style::default().fg(app.theme.muted_color()),
        ));

        let scroll_widget = ratatui::widgets::Paragraph::new(scroll_text);
        f.render_widget(scroll_widget, scroll_area);
    }

    Ok(())
}

fn create_message_row<'a>(msg: &'a DecodedMessageView, app: &App) -> Row<'a> {
    // Cross-panel global focus (Task 2): when some OTHER panel is active and
    // this row's callsign is the operator's current focus, flag it here too
    // — but only when Band Activity itself is NOT the active panel, so we
    // never double up with the active panel's REVERSED row-highlight.
    let is_active_panel = matches!(app.active_panel, ActivePanel::BandActivity);
    let is_globally_focused =
        !is_active_panel && msg.call_sign.as_deref().is_some_and(|c| app.is_focused(c));

    // Always show HH:MM:SS in UTC — FT8 timing needs seconds granularity
    let time_short = msg.timestamp.format("%H:%M:%S").to_string();

    let snr_str = format!("{:+}", msg.snr);
    let dt_str = format!("{:+.1}", msg.delta_time);
    let df_str = format!("{:+.0}", msg.delta_freq);

    // Tier-2 highlight (Task 3): one of our current active-QSO partners.
    // Directed-at-us styling still wins, so this only applies otherwise.
    let is_engaged =
        !msg.is_directed_at_us && msg.call_sign.as_deref().is_some_and(|c| app.is_engaged(c));

    // Lead the call column with "→" for directed-at-us decodes so even
    // colorblind / monochrome terminals can spot them at a glance, or "● "
    // for an engaged (active-QSO) station.
    let call_str = match msg.call_sign.as_deref() {
        Some(c) if msg.is_directed_at_us => format!("→ {}", c),
        Some(c) if is_engaged => format!("● {}", c),
        Some(c) => c.to_string(),
        None => "---".to_string(),
    };
    let grid_str = msg.grid_square.as_deref().unwrap_or("---");
    let dist_str = format_distance(msg.distance);

    // Truncate long messages. Raised from 30 to 60 chars (Task 20a) — the
    // Msg column absorbed the space freed by dropping the per-row-constant
    // Freq/Mode columns.
    let msg_str = if msg.message.len() > 60 {
        format!("{}...", &msg.message[..57])
    } else {
        msg.message.clone()
    };

    // "Calling me" rows take priority over CQ highlighting; the entire row
    // gets the selected_color background-equivalent treatment so they're
    // visually distinct from CQs on the same screen.
    let directed_style = Style::default()
        .fg(app.theme.selected_color())
        .add_modifier(Modifier::BOLD);

    let snr_style = if msg.is_directed_at_us {
        directed_style
    } else {
        Style::default().fg(get_snr_color(msg.snr, &app.theme))
    };

    let call_style = if msg.is_directed_at_us {
        directed_style
    } else if is_globally_focused {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else if msg.worked_before {
        // Already in the log on this band — dim the callsign the same
        // way the DX hunter panel does, so the operator's eye skips
        // stations the autonomous scorer would also dup-penalize.
        Style::default().fg(app.theme.muted_color())
    } else if msg.call_sign.is_some() {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted_color())
    };
    // Tier-2 (engaged) highlight stacks on top of whatever the above chose —
    // green + underline, without discarding e.g. the tier-1 BOLD.
    let call_style = if is_engaged {
        call_style
            .fg(app.theme.success_color())
            .add_modifier(Modifier::UNDERLINED)
    } else {
        call_style
    };

    let msg_style = if msg.is_directed_at_us {
        directed_style
    } else if msg.message.contains("CQ") {
        Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.foreground_color())
    };

    let neutral_style = if msg.is_directed_at_us {
        directed_style
    } else {
        Style::default().fg(app.theme.foreground_color())
    };
    let muted_style = if msg.is_directed_at_us {
        directed_style
    } else {
        Style::default().fg(app.theme.muted_color())
    };

    Row::new([
        Cell::from(time_short).style(muted_style),
        Cell::from(snr_str).style(snr_style),
        Cell::from(dt_str).style(neutral_style),
        Cell::from(df_str).style(neutral_style),
        Cell::from(call_str).style(call_style),
        Cell::from(grid_str).style(neutral_style),
        Cell::from(dist_str).style(neutral_style),
        Cell::from(msg_str).style(msg_style),
    ])
}

/// Helper to determine if a message is interesting (CQ, directed to us, etc.)
pub fn is_interesting_message(msg: &DecodedMessageView, our_call: &str) -> bool {
    let message_upper = msg.message.to_uppercase();
    let our_call_upper = our_call.to_uppercase();

    // Check if message contains our call sign
    if message_upper.contains(&our_call_upper) {
        return true;
    }

    // Check if it's a CQ call
    if message_upper.starts_with("CQ") {
        return true;
    }

    // Check if it's a new DXCC entity (would need DXCC database)
    // For now, just check if it has good SNR and distance
    if msg.snr > 0 && msg.distance.unwrap_or(0.0) > 1000.0 {
        return true;
    }

    false
}

/// Extract callsign from various message formats
pub fn extract_callsign_from_message(message: &str) -> Option<String> {
    let parts: Vec<&str> = message.split_whitespace().collect();

    if parts.is_empty() {
        return None;
    }

    // Handle CQ messages: "CQ DX K1ABC FN42"
    if parts[0] == "CQ" && parts.len() >= 3 {
        return Some(parts[2].to_string());
    }

    // Handle exchange messages: "K1ABC W2XYZ RRR"
    if parts.len() >= 2 {
        // First part might be a callsign
        if is_valid_callsign(parts[0]) {
            return Some(parts[0].to_string());
        }
        // Second part might be a callsign
        if is_valid_callsign(parts[1]) {
            return Some(parts[1].to_string());
        }
    }

    None
}

/// Basic callsign validation
fn is_valid_callsign(s: &str) -> bool {
    // Very basic check: contains letters and numbers, reasonable length
    s.len() >= 3
        && s.len() <= 10
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_extract_callsign_from_cq() {
        let message = "CQ DX K1ABC FN42";
        assert_eq!(
            extract_callsign_from_message(message),
            Some("K1ABC".to_string())
        );
    }

    #[test]
    fn test_extract_callsign_from_exchange() {
        let message = "K1ABC W2XYZ -15";
        assert_eq!(
            extract_callsign_from_message(message),
            Some("K1ABC".to_string())
        );
    }

    #[test]
    fn test_valid_callsign() {
        assert!(is_valid_callsign("K1ABC"));
        assert!(is_valid_callsign("VK4AAA"));
        assert!(is_valid_callsign("JA1XYZ"));
        assert!(!is_valid_callsign("123"));
        assert!(!is_valid_callsign("ABC"));
        assert!(!is_valid_callsign(""));
    }
}
