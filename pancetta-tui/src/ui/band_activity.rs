use super::need_display::{atno_style, band_marker, need_marker, needed_style};
use crate::dx_need::canonical_grid_square;
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
            // Need markers plus directed/engaged prefix and a portable call.
            BandActivityColumn::Call => Constraint::Length(12),
            BandActivityColumn::Grid => Constraint::Length(5),
            BandActivityColumn::Dist => Constraint::Length(6),
            BandActivityColumn::Msg => Constraint::Min(20),
        }
    }
}

/// Adaptive column set for a given panel width (Task 20b). Below 77 cols,
/// `Dist`+`Grid` are dropped (least essential for a quick scan); below 64,
/// `DT`+`DF` go too (fine sync/freq offsets an operator can live without at
/// that width) — leaving Time/SNR/Call/Msg as the irreducible core.
// Budgets include borders, column gaps and the two-cell selection marker.
fn visible_columns(width: u16) -> Vec<BandActivityColumn> {
    let mut cols: Vec<BandActivityColumn> = BandActivityColumn::ALL.to_vec();
    if width < 77 {
        cols.retain(|c| !matches!(c, BandActivityColumn::Grid | BandActivityColumn::Dist));
    }
    if width < 64 {
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

    // #172: own-TX rows have no meaningful RX SNR (snr is a 0 sentinel);
    // render "TX" instead so it's never mistaken for a real 0 dB decode —
    // same not-color-only precedent as dx_hunter.rs::format_dx_snr's "---".
    let snr_str = if msg.is_own_tx {
        "TX".to_string()
    } else {
        format!("{:+}", msg.snr)
    };
    let dt_str = format!("{:+.1}", msg.delta_time);
    let df_str = format!("{:+.0}", msg.delta_freq);

    // Tier-2 highlight (Task 3): one of our current active-QSO partners.
    // Directed-at-us styling still wins, so this only applies otherwise.
    let is_engaged =
        !msg.is_directed_at_us && msg.call_sign.as_deref().is_some_and(|c| app.is_engaged(c));

    // Lead the call column with "→" for directed-at-us decodes so even
    // colorblind / monochrome terminals can spot them at a glance, "● "
    // for an engaged (active-QSO) station, or "» TX" for a frame WE
    // transmitted (#172) — call_sign is always None for these rows, the
    // full exchange text is in the Msg column.
    let (needed, atno) = msg
        .call_sign
        .as_deref()
        .filter(|_| !msg.is_own_tx)
        .map(|call| app.dx_need_flags_shown(call, msg.needed, msg.atno))
        .unwrap_or((false, false));
    let call_str = if msg.is_own_tx {
        "» TX".to_string()
    } else {
        match msg.call_sign.as_deref() {
            Some(c) if msg.is_directed_at_us => format!("→ {}", c),
            Some(c) if is_engaged => format!("● {}", c),
            Some(c) => c.to_string(),
            None => "---".to_string(),
        }
    };
    let call_str = if !msg.is_own_tx && msg.call_sign.is_some() {
        format!(
            "{}{}{}",
            need_marker(atno, needed, false),
            band_marker(msg.band_needed, atno),
            call_str
        )
    } else {
        call_str
    };
    let grid = msg.grid_square.as_deref().and_then(canonical_grid_square);
    // A frame's grid verdict answers for the current DX row, not an older
    // decode of a rover at a different locator. Use the exact normalized key.
    let grid_needed = !msg.is_own_tx
        && grid.as_ref().is_some_and(|square| {
            msg.call_sign.as_deref().is_some_and(|call| {
                let key = pancetta_core::callsign::resolve_hash_render(call).unwrap_or(call);
                app.dx_stations
                    .get(key)
                    .and_then(|station| station.grid_square.as_deref())
                    .and_then(canonical_grid_square)
                    .as_ref()
                    == Some(square)
                    && app
                        .dx_need_for(call)
                        .is_some_and(|need| need.grid_needed == Some(true))
            })
        });
    let grid_str = match grid {
        Some(square) if grid_needed => format!("+{square}"),
        Some(square) => square,
        None => "---".into(),
    };
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

    // #172: distinct style for own-TX rows so they read as ours at a
    // glance, checked before the existing is_directed_at_us styling
    // (own-TX rows are always is_directed_at_us: true too, but the "»"/
    // "TX" text markers already carry the meaning independent of color —
    // this is additional, not the only, signal).
    let own_tx_style = Style::default()
        .fg(ratatui::style::Color::Red)
        .add_modifier(Modifier::BOLD);

    let snr_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
        directed_style
    } else {
        Style::default().fg(get_snr_color(msg.snr, &app.theme))
    };

    let call_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
        directed_style
    } else if is_globally_focused {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else if atno {
        atno_style()
    } else if msg.worked_before {
        // Already in the log on this band — dim the callsign the same
        // way the DX hunter panel does, so the operator's eye skips
        // stations the autonomous scorer would also dup-penalize.
        Style::default().fg(app.theme.muted_color())
    } else if needed {
        needed_style(&app.theme)
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

    let msg_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
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
            Cell::from(grid_str).style(if grid_needed {
                needed_style(&app.theme)
            } else {
                neutral_style
            }),
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

    struct Need(crate::dx_need::DxNeedStatus);
    impl crate::dx_need::DxNeedLookup for Need {
        fn lookup_need(
            &self,
            _: &str,
            _: Option<&'static str>,
            _: Option<&str>,
            _: Option<&str>,
            _: f64,
            _: Option<&str>,
        ) -> crate::dx_need::DxNeedStatus {
            self.0.clone()
        }
    }

    async fn need_app(msg: DecodedMessageView) -> App {
        let mut app = App::new(crate::Config::default(), None).await.unwrap();
        app.add_decoded_message(msg).await.unwrap();
        app.active_panel = ActivePanel::Callers;
        app
    }

    fn render_need(app: &App, width: u16, hunter: bool) -> ratatui::buffer::Buffer {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 8)).unwrap();
        term.draw(|f| {
            if hunter {
                crate::ui::dx_hunter::render_dx_hunter(f, f.area(), app).unwrap();
            } else {
                render_band_activity(f, f.area(), app).unwrap();
            }
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    fn token_cell<'a>(buf: &'a ratatui::buffer::Buffer, token: &str) -> &'a ratatui::buffer::Cell {
        let row = row_text(buf, 2);
        let byte = row
            .find(token)
            .unwrap_or_else(|| panic!("missing {token:?}: {row}"));
        &buf[(row[..byte].chars().count() as u16, 2)]
    }

    fn call_text(buf: &ratatui::buffer::Buffer) -> String {
        let header = header_row_text(buf);
        let byte = header.find("Call").unwrap();
        let x = header[..byte].chars().count() as u16;
        (x..x + 12)
            .map(|x| buf[(x, 2)].symbol())
            .collect::<String>()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn pan350_call_need_parity() {
        use ratatui::style::Color;
        for (needed, atno, band, worked, prefix) in [
            (true, true, true, false, "!"),
            (true, false, true, false, "+▲"),
            (false, false, true, false, "▲"),
            (false, false, false, false, ""),
            (true, false, false, true, "+"),
            (true, true, false, true, "!"),
        ] {
            let mut msg = fixture_message("ZL1ABC");
            msg.needed = needed;
            msg.atno = atno;
            msg.band_needed = band;
            msg.worked_before = worked;
            let app = need_app(msg).await;
            let token = format!("{prefix}ZL1ABC");
            let band = render_need(&app, 220, false);
            let dx = render_need(&app, 220, true);
            assert_eq!(call_text(&band), token);
            assert_eq!(call_text(&dx), token);
            let a = token_cell(&band, &token);
            let b = token_cell(&dx, &token);
            assert_eq!((a.fg, a.modifier), (b.fg, b.modifier));
            assert_eq!(
                a.fg,
                if atno {
                    Color::Magenta
                } else if worked {
                    app.theme.muted_color()
                } else if needed {
                    app.theme.error_color()
                } else {
                    app.theme.success_color()
                }
            );
            assert_eq!(
                a.modifier,
                if atno {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else if worked {
                    Modifier::empty()
                } else {
                    Modifier::BOLD
                }
            );
        }
    }

    #[tokio::test]
    async fn pan350_live_need_and_grid_parity() {
        for (stored, atno, verdict, prefix) in [
            (true, true, Some(false), ""),
            (false, false, Some(true), "+"),
            (true, true, None, "!"),
            (true, false, None, "+"),
            (false, false, None, ""),
        ] {
            for call in ["ZL1ABC", "<ZL1ABC>"] {
                let mut msg = fixture_message(call);
                msg.needed = stored;
                msg.atno = atno;
                msg.grid_square = Some("rf72ab".into());
                let mut app = need_app(msg).await;
                for grid_need in [Some(true), Some(false), None] {
                    app.refresh_dx_need(Some(&Need(crate::dx_need::DxNeedStatus {
                        dxcc_needed: verdict,
                        grid_needed: grid_need,
                        ..Default::default()
                    })));
                    let band = render_need(&app, 220, false);
                    let dx = render_need(&app, 220, true);
                    assert_eq!(call_text(&band), format!("{prefix}{call}"));
                    assert_eq!(call_text(&dx), format!("{prefix}ZL1ABC"));
                    let a = token_cell(&band, &format!("{prefix}{call}"));
                    let b = token_cell(&dx, &format!("{prefix}ZL1ABC"));
                    assert_eq!((a.fg, a.modifier), (b.fg, b.modifier));
                    let grid = if grid_need == Some(true) {
                        "+RF72"
                    } else {
                        "RF72"
                    };
                    for buf in [&band, &dx] {
                        let c = token_cell(buf, grid);
                        if grid_need == Some(true) {
                            assert_eq!(c.fg, app.theme.error_color());
                            assert!(c.modifier.contains(Modifier::BOLD));
                        } else {
                            assert!(!row_text(buf, 2).contains("+RF72"));
                        }
                    }
                }
            }
        }
        let mut msg = fixture_message("ZL1ABC");
        msg.grid_square = Some("rf72ab".into());
        let mut app = need_app(msg).await;
        app.refresh_dx_need(Some(&Need(crate::dx_need::DxNeedStatus {
            grid_needed: Some(true),
            ..Default::default()
        })));
        app.dx_stations.get_mut("ZL1ABC").unwrap().grid_square = Some("FN42".into());
        assert!(!row_text(&render_need(&app, 120, false), 2).contains("+RF72"));
        app.dx_stations.clear();
        assert!(!row_text(&render_need(&app, 120, false), 2).contains("+RF72"));
        for invalid in [None, Some("ZZ34"), Some("RF72☃")] {
            app.decoded_messages.front_mut().unwrap().grid_square = invalid.map(str::to_string);
            let buf = render_need(&app, 120, false);
            token_cell(&buf, "---");
            assert!(!row_text(&buf, 2).contains("+RF72"));
        }
    }

    #[tokio::test]
    async fn pan350_marker_widths_and_overrides() {
        let mut msg = fixture_message("ZL1ABC/P");
        msg.needed = true;
        msg.band_needed = true;
        msg.is_directed_at_us = true;
        msg.grid_square = Some("rf72ab".into());
        msg.message = "CQ ZL1ABC/P RF72".into();
        let mut app = need_app(msg).await;
        app.refresh_dx_need(Some(&Need(crate::dx_need::DxNeedStatus {
            grid_needed: Some(true),
            ..Default::default()
        })));
        for width in [120, 77, 76, 70, 65, 64, 63, 55, 51] {
            let buf = render_need(&app, width, false);
            let cell = token_cell(&buf, "+▲→ ZL1ABC/P");
            assert_eq!(cell.fg, app.theme.selected_color());
            token_cell(&buf, "CQ ZL1ABC/P RF72");
            assert_eq!(header_row_text(&buf).contains("Grid"), width >= 77);
            assert_eq!(header_row_text(&buf).contains("DT"), width >= 64);
            if width >= 77 {
                token_cell(&buf, "+RF72");
            }
        }
        app.active_panel = ActivePanel::BandActivity;
        let buf = render_need(&app, 120, false);
        let c = token_cell(&buf, "+▲→ ZL1ABC/P");
        assert_eq!(c.fg, ratatui::style::Color::Black);
        assert_eq!(c.bg, app.theme.accent_color());
        app.active_panel = ActivePanel::Callers;
        app.decoded_messages.front_mut().unwrap().is_directed_at_us = false;
        app.active_qsos.push(fixture_banner("ZL1ABC/P"));
        let buf = render_need(&app, 120, false);
        let c = token_cell(&buf, "+▲● ZL1ABC/P");
        assert_eq!(c.fg, app.theme.success_color());
        assert!(c.modifier.contains(Modifier::UNDERLINED));
        app.active_qsos.clear();
        app.active_panel = ActivePanel::DxHunter;
        let buf = render_need(&app, 120, false);
        assert_eq!(
            token_cell(&buf, "+▲ZL1ABC/P").fg,
            app.theme.selected_color()
        );
        app.active_panel = ActivePanel::Callers;
        let row = app.decoded_messages.front_mut().unwrap();
        row.is_own_tx = true;
        row.call_sign = None;
        token_cell(&render_need(&app, 120, false), "» TX");
        app.decoded_messages.front_mut().unwrap().is_own_tx = false;
        token_cell(&render_need(&app, 120, false), "---");
        app.decoded_messages.clear();
        let buf = render_need(&app, 120, false);
        token_cell(&buf, "No messages");
        token_cell(&buf, "Monitoring...");
        for width in [0, 1, 2, 3, 10] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 8)).unwrap();
            terminal
                .draw(|f| render_band_activity(f, Rect::new(0, 0, width, 0), &app).unwrap())
                .unwrap();
        }
    }

    fn fixture_banner(call: &str) -> crate::app::ActiveQsoBanner {
        crate::app::ActiveQsoBanner {
            their_callsign: call.into(),
            state: "wait rpt".into(),
            started_at: chrono::Utc::now(),
            frequency_hz: 1234.0,
            tx_parity: None,
            last_tx_text: None,
            last_tx_at: None,
            last_rx_text: None,
            last_rx_at: None,
            snr_rx: None,
            report_sent: None,
            report_received: None,
            exchange_count: 0,
            qso_id: format!("{call}-id"),
            initiated_by: "Manual".into(),
            ladder_labels: Vec::new(),
            ladder_ours: Vec::new(),
            ladder_index: 0,
            now_line: String::new(),
            next_line: String::new(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
            dx_last_activity: None,
            hound: false,
            pending_freq_drift_hz: None,
            pending_freq_drift_since: None,
        }
    }

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
    fn visible_columns_drops_dist_grid_below_77() {
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
    fn visible_columns_also_drops_dt_df_below_64() {
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
            priority_factors: None,
            is_own_tx: false,
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

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    fn header_row_text(buf: &ratatui::buffer::Buffer) -> String {
        row_text(buf, 1)
    }

    #[tokio::test]
    async fn own_tx_row_renders_marker_and_tx_snr() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let mut tx_row = fixture_message("K5ARH");
        tx_row.is_own_tx = true;
        tx_row.is_directed_at_us = true;
        tx_row.call_sign = None;
        tx_row.message = "K5ARH JA1ABC RR73".to_string();
        app.decoded_messages.push_back(tx_row);

        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_band_activity(f, area, &app).unwrap();
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        // Row 0 = block border/title, row 1 = header, row 2 = first data row.
        let data_row = row_text(&buf, 2);
        assert!(data_row.contains("» TX"), "marker missing: {data_row:?}");
        assert!(
            data_row.contains("RR73"),
            "message text missing: {data_row:?}"
        );
    }

    #[tokio::test]
    async fn rx_row_unaffected_by_own_tx_marker() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.decoded_messages.push_back(fixture_message("K1ABC"));

        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_band_activity(f, area, &app).unwrap();
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let data_row = row_text(&buf, 2);
        assert!(
            !data_row.contains("» TX"),
            "unexpected TX marker: {data_row:?}"
        );
        assert!(data_row.contains("K1ABC"), "callsign missing: {data_row:?}");
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
