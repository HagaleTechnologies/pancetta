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

/// Single source of truth for Band Activity's column set (Task 20b). Header
/// labels, cell widths, and per-row cell selection all derive from this enum
/// via [`visible_columns`], so the three width-tier shapes (full / narrow /
/// very-narrow) can never drift out of sync with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandActivityColumn {
    Time,
    Snr,
    Dt,
    Df,
    Call,
    Grid,
    Dist,
    Msg,
}

impl BandActivityColumn {
    /// Canonical column order — also the order every consumer (header,
    /// widths, row cells) iterates in.
    const ALL: [BandActivityColumn; 8] = [
        BandActivityColumn::Time,
        BandActivityColumn::Snr,
        BandActivityColumn::Dt,
        BandActivityColumn::Df,
        BandActivityColumn::Call,
        BandActivityColumn::Grid,
        BandActivityColumn::Dist,
        BandActivityColumn::Msg,
    ];

    fn header(self) -> &'static str {
        match self {
            BandActivityColumn::Time => "Time",
            BandActivityColumn::Snr => "SNR",
            BandActivityColumn::Dt => "DT",
            BandActivityColumn::Df => "DF",
            BandActivityColumn::Call => "Call",
            BandActivityColumn::Grid => "Grid",
            BandActivityColumn::Dist => "Dist",
            BandActivityColumn::Msg => "Msg",
        }
    }

    fn width(self) -> Constraint {
        match self {
            BandActivityColumn::Time => Constraint::Length(8),
            BandActivityColumn::Snr => Constraint::Length(4),
            BandActivityColumn::Dt => Constraint::Length(5), // fits "-15.2"
            BandActivityColumn::Df => Constraint::Length(6), // fits "+2800"
            // Widened to absorb the engaged "● " prefix.
            BandActivityColumn::Call => Constraint::Length(10),
            BandActivityColumn::Grid => Constraint::Length(4),
            BandActivityColumn::Dist => Constraint::Length(6),
            BandActivityColumn::Msg => Constraint::Min(20),
        }
    }
}

/// Adaptive column set for a given panel width (Task 20b). Below 70 cols,
/// `Dist`+`Grid` are dropped (least essential for a quick scan); below 55,
/// `DT`+`DF` go too (fine sync/freq offsets an operator can live without at
/// that width) — leaving Time/SNR/Call/Msg as the irreducible core.
fn visible_columns(width: u16) -> Vec<BandActivityColumn> {
    let mut cols: Vec<BandActivityColumn> = BandActivityColumn::ALL.to_vec();
    if width < 70 {
        cols.retain(|c| !matches!(c, BandActivityColumn::Grid | BandActivityColumn::Dist));
    }
    if width < 55 {
        cols.retain(|c| !matches!(c, BandActivityColumn::Dt | BandActivityColumn::Df));
    }
    cols
}

pub fn render_band_activity(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::BandActivity);
    let block = create_panel_block("Band Activity", is_active, app);

    // Task 20a/20b: `Freq`/`Mode` were dropped outright (per-row-constant,
    // no information); the remaining columns adaptively hide at narrow
    // widths — see `visible_columns`.
    let columns = visible_columns(area.width);

    let header_cells = columns.iter().map(|c| {
        Cell::from(c.header()).style(
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
        .map(|msg| create_message_row(msg, app, &columns))
        .collect();

    // If no messages, show placeholder
    if rows.is_empty() {
        rows.push(Row::new(columns.iter().map(|c| {
            let text = match c {
                BandActivityColumn::Call => "No messages",
                BandActivityColumn::Msg => "Monitoring...",
                _ => "",
            };
            Cell::from(text).style(Style::default().fg(app.theme.muted_color()))
        })));
    }

    let widths: Vec<Constraint> = columns.iter().map(|c| c.width()).collect();

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
    if is_active && !displayed.is_empty() {
        table_state.select(Some(app.band_activity_scroll));
    }

    f.render_stateful_widget(table, area, &mut table_state);

    // Show scroll indicator if there are more messages. Uses `displayed`
    // (the view-filtered count, e.g. Hunt's CQs-only filter) rather than
    // the raw `app.decoded_messages` count, so the "N/total" denominator
    // matches what's actually visible in this view.
    if displayed.len() > (area.height as usize).saturating_sub(4) {
        let scroll_info = format!("{}/{}", app.band_activity_scroll + 1, displayed.len());

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

fn create_message_row<'a>(
    msg: &'a DecodedMessageView,
    app: &App,
    columns: &[BandActivityColumn],
) -> Row<'a> {
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
    //
    // FT8 payloads are ASCII in practice (callsigns/grids/reports), so
    // `..57` always lands on a char boundary today — but byte-slicing a
    // `str` panics if it ever doesn't, so walk back to the nearest valid
    // boundary first (issue #98 item 3) rather than trust that invariant.
    let msg_str = if msg.message.len() > 60 {
        let mut cut = 57.min(msg.message.len());
        while cut > 0 && !msg.message.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &msg.message[..cut])
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

    // Build every cell in canonical column order, then keep only the ones
    // this width tier wants — same `columns` list the header/widths were
    // built from, so the row can never end up with a different shape.
    let all_cells: [(BandActivityColumn, Cell<'a>); 8] = [
        (
            BandActivityColumn::Time,
            Cell::from(time_short).style(muted_style),
        ),
        (
            BandActivityColumn::Snr,
            Cell::from(snr_str).style(snr_style),
        ),
        (
            BandActivityColumn::Dt,
            Cell::from(dt_str).style(neutral_style),
        ),
        (
            BandActivityColumn::Df,
            Cell::from(df_str).style(neutral_style),
        ),
        (
            BandActivityColumn::Call,
            Cell::from(call_str).style(call_style),
        ),
        (
            BandActivityColumn::Grid,
            Cell::from(grid_str).style(neutral_style),
        ),
        (
            BandActivityColumn::Dist,
            Cell::from(dist_str).style(neutral_style),
        ),
        (
            BandActivityColumn::Msg,
            Cell::from(msg_str).style(msg_style),
        ),
    ];

    Row::new(
        all_cells
            .into_iter()
            .filter(|(col, _)| columns.contains(col))
            .map(|(_, cell)| cell),
    )
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

    // --- Task 20b: adaptive column hiding ------------------------------

    #[test]
    fn visible_columns_full_at_wide_width() {
        let cols = visible_columns(120);
        assert_eq!(cols, BandActivityColumn::ALL.to_vec());
    }

    #[test]
    fn visible_columns_drops_dist_grid_below_70() {
        let cols = visible_columns(65);
        assert!(!cols.contains(&BandActivityColumn::Grid));
        assert!(!cols.contains(&BandActivityColumn::Dist));
        // DT/DF/Call/Msg/Time/SNR still present at this tier.
        assert!(cols.contains(&BandActivityColumn::Dt));
        assert!(cols.contains(&BandActivityColumn::Df));
        assert!(cols.contains(&BandActivityColumn::Call));
        assert!(cols.contains(&BandActivityColumn::Msg));
    }

    #[test]
    fn visible_columns_also_drops_dt_df_below_55() {
        let cols = visible_columns(50);
        assert!(!cols.contains(&BandActivityColumn::Grid));
        assert!(!cols.contains(&BandActivityColumn::Dist));
        assert!(!cols.contains(&BandActivityColumn::Dt));
        assert!(!cols.contains(&BandActivityColumn::Df));
        // Irreducible core.
        assert_eq!(
            cols,
            vec![
                BandActivityColumn::Time,
                BandActivityColumn::Snr,
                BandActivityColumn::Call,
                BandActivityColumn::Msg,
            ]
        );
    }

    fn fixture_message(call: &str) -> DecodedMessageView {
        DecodedMessageView {
            timestamp: Utc::now(),
            frequency: 14.074,
            mode: "FT8".to_string(),
            snr: -10,
            delta_time: 0.1,
            delta_freq: 1500.0,
            call_sign: Some(call.to_string()),
            grid_square: Some("FN42".to_string()),
            message: format!("CQ {} FN42", call),
            distance: Some(1234.0),
            bearing: None,
            slot_parity: None,
            is_directed_at_us: false,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: None,
        }
    }

    async fn render_panel_at_width(width: u16) -> ratatui::buffer::Buffer {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.decoded_messages.push_back(fixture_message("K1ABC"));

        let backend = TestBackend::new(width, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_band_activity(f, area, &app).unwrap();
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    fn header_row_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[tokio::test]
    async fn render_shows_all_columns_at_width_120() {
        let buf = render_panel_at_width(120).await;
        let header = header_row_text(&buf);
        assert!(header.contains("Grid"), "header: {header:?}");
        assert!(header.contains("Dist"), "header: {header:?}");
        assert!(header.contains("DT"), "header: {header:?}");
        assert!(header.contains("DF"), "header: {header:?}");
    }

    #[tokio::test]
    async fn render_drops_grid_dist_at_width_65() {
        let buf = render_panel_at_width(65).await;
        let header = header_row_text(&buf);
        assert!(!header.contains("Grid"), "header: {header:?}");
        assert!(!header.contains("Dist"), "header: {header:?}");
        assert!(header.contains("DT"), "header: {header:?}");
        assert!(header.contains("DF"), "header: {header:?}");
    }

    #[tokio::test]
    async fn render_drops_dt_df_grid_dist_at_width_50() {
        let buf = render_panel_at_width(50).await;
        let header = header_row_text(&buf);
        assert!(!header.contains("Grid"), "header: {header:?}");
        assert!(!header.contains("Dist"), "header: {header:?}");
        assert!(!header.contains("DT"), "header: {header:?}");
        assert!(!header.contains("DF"), "header: {header:?}");
        assert!(header.contains("Time"), "header: {header:?}");
        assert!(header.contains("SNR"), "header: {header:?}");
        assert!(header.contains("Call"), "header: {header:?}");
        assert!(header.contains("Msg"), "header: {header:?}");
    }

    /// Issue #98 item 3: a multi-byte char straddling byte offset 57 used
    /// to panic on `&msg.message[..57]`. Not reachable with real FT8
    /// payloads (ASCII only), but the truncation must not panic if it ever
    /// is. 56 ASCII chars + a 3-byte char (spanning bytes 56-59, so byte 57
    /// is NOT a char boundary) + padding past the 60-char truncation
    /// threshold.
    #[tokio::test]
    async fn create_message_row_does_not_panic_on_multibyte_boundary() {
        let mut msg = fixture_message("K1ABC");
        msg.message = format!("{}日{}", "A".repeat(56), "B".repeat(10));
        let app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let _ = create_message_row(&msg, &app, &BandActivityColumn::ALL);
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
