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

/// Row-highlight marker for the selected caller. Also part of the column
/// budget in [`visible_columns`] (PAN-88, D4).
const HIGHLIGHT_SYMBOL: &str = "▶ ";

/// Single source of truth for the Callers column set (PAN-88), mirroring
/// Band Activity's `BandActivityColumn`: header, widths and row cells all
/// derive from [`visible_columns`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallersColumn {
    Call,
    Msg,
    Snr,
    Freq,
    Age,
    Flags,
}

impl CallersColumn {
    /// Display order (left to right).
    pub(super) const ALL: [CallersColumn; 6] = [
        CallersColumn::Call,
        CallersColumn::Msg,
        CallersColumn::Snr,
        CallersColumn::Freq,
        CallersColumn::Age,
        CallersColumn::Flags,
    ];

    /// The order columns join as the panel widens. `BUSY` (working someone
    /// else) and `WKD` (a dupe) decide whether to answer; the caller's audio
    /// offset doesn't, because Space replies on our own offset.
    const PRIORITY: [CallersColumn; 6] = [
        CallersColumn::Call,
        CallersColumn::Msg,
        CallersColumn::Snr,
        CallersColumn::Flags,
        CallersColumn::Age,
        CallersColumn::Freq,
    ];

    pub(super) fn header(self) -> &'static str {
        match self {
            CallersColumn::Call => "Call",
            CallersColumn::Msg => "Msg",
            CallersColumn::Snr => "SNR",
            CallersColumn::Freq => "Freq",
            CallersColumn::Age => "Age",
            CallersColumn::Flags => "Flags",
        }
    }

    fn width(self) -> Constraint {
        match self {
            CallersColumn::Call => Constraint::Min(8),
            CallersColumn::Msg => Constraint::Min(10),
            CallersColumn::Snr => Constraint::Length(4),
            CallersColumn::Freq => Constraint::Length(5),
            CallersColumn::Age => Constraint::Length(4),
            // Fits two flags ("BUSY WKD"); the rare three-flag case reads
            // "BUSY MINE" (PAN-88 narrowed this from 12).
            CallersColumn::Flags => Constraint::Length(9),
        }
    }

    /// The narrowest this column is ever drawn (its `Length`/`Min`).
    fn min_width(self) -> u16 {
        match self.width() {
            Constraint::Length(n) | Constraint::Min(n) => n,
            _ => 0,
        }
    }
}

/// Columns shown in a Callers panel `width` columns wide (PAN-88): the same
/// prefix rule and highlight-marker reservation as DX Hunter's
/// `visible_columns` — walk [`CallersColumn::PRIORITY`] and stop at the first
/// column that doesn't fit, so no heading is squeezed to a stub. `Call` is
/// always shown. Returned in display order.
pub(super) fn visible_columns(width: u16) -> Vec<CallersColumn> {
    let budget = width
        .saturating_sub(2)
        .saturating_sub(Span::raw(HIGHLIGHT_SYMBOL).width() as u16);
    let n = super::bar_fit::prefix_fit(
        CallersColumn::PRIORITY.iter().map(|c| c.min_width()),
        budget,
    )
    .max(1);
    let shown = &CallersColumn::PRIORITY[..n];
    CallersColumn::ALL
        .into_iter()
        .filter(|c| shown.contains(c))
        .collect()
}

pub fn render_callers(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::Callers);
    let block = create_panel_block("Callers", is_active, app);

    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() {
        return Ok(());
    }

    // Keep the outer-width column budget, including the border reservation.
    let columns = visible_columns(area.width);
    let header_cells = columns.iter().map(|c| {
        Cell::from(c.header()).style(
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
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };

    let mut rows: Vec<Row> = callers
        .iter()
        .map(|m| caller_row(m, app, &columns))
        .collect();
    if rows.is_empty() {
        rows.push(Row::new(columns.iter().map(|c| {
            let text = match c {
                CallersColumn::Call => "No one",
                CallersColumn::Msg => "calling",
                _ => "",
            };
            Cell::from(text).style(Style::default().fg(app.theme.muted_color()))
        })));
    }

    let widths: Vec<Constraint> = columns.iter().map(|c| c.width()).collect();

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()))
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(HIGHLIGHT_SYMBOL);

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

fn caller_row<'a>(msg: &'a DecodedMessageView, app: &App, columns: &[CallersColumn]) -> Row<'a> {
    let call = msg.call_sign.clone().unwrap_or_default();

    // Cross-panel global focus (Task 2): when some OTHER panel is active and
    // this row's callsign is the operator's current focus, flag it here too
    // — but only when Callers itself is NOT the active panel, so we never
    // double up with the active panel's REVERSED row-highlight.
    let is_active_panel = matches!(app.active_panel, ActivePanel::Callers);
    let is_globally_focused = !is_active_panel && !call.is_empty() && app.is_focused(&call);

    // Tier-2 highlight (Task 3): one of our current active-QSO partners.
    let is_engaged = !call.is_empty() && app.is_engaged(&call);

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

    let call_display = if is_engaged {
        format!("● {}", call)
    } else {
        call.clone()
    };

    let dim = Style::default().fg(app.theme.foreground_color());
    let call_style = if is_globally_focused {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD)
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
    let snr_style = Style::default().fg(super::get_snr_color(msg.snr, &app.theme));
    let flags_style = if flags.contains(&"BUSY") {
        Style::default().fg(app.theme.warning_color())
    } else {
        Style::default().fg(app.theme.muted_color())
    };

    // One cell per visible column (PAN-88).
    Row::new(
        [
            (
                CallersColumn::Call,
                Cell::from(call_display).style(call_style),
            ),
            (CallersColumn::Msg, Cell::from(text).style(dim)),
            (CallersColumn::Snr, Cell::from(snr_str).style(snr_style)),
            (
                CallersColumn::Freq,
                Cell::from(freq_str).style(Style::default().fg(app.theme.accent_color())),
            ),
            (
                CallersColumn::Age,
                Cell::from(age_str).style(Style::default().fg(app.theme.muted_color())),
            ),
            (
                CallersColumn::Flags,
                Cell::from(flags_str).style(flags_style),
            ),
        ]
        .into_iter()
        .filter(|(col, _)| columns.contains(col))
        .map(|(_, cell)| cell),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller_fixture(call: &str) -> DecodedMessageView {
        DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14_074_000.0,
            mode: "FT8".into(),
            snr: -15,
            delta_time: 0.0,
            delta_freq: 1500.0,
            call_sign: Some(call.into()),
            grid_square: None,
            message: format!("N0CALL {call} -10"),
            distance: None,
            bearing: None,
            slot_parity: None,
            is_directed_at_us: true,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: None,
            priority_factors: None,
            is_own_tx: false,
        }
    }

    #[tokio::test]
    async fn callers_hint_is_inside_intact_border_at_small_sizes() {
        use ratatui::{backend::TestBackend, Terminal};
        for populated in [false, true] {
            let mut app = App::new(crate::config::Config::default(), None)
                .await
                .unwrap();
            app.active_panel = ActivePanel::Callers;
            if populated {
                for i in 0..8 {
                    app.decoded_messages
                        .push_back(caller_fixture(&format!("K{i}ABC")));
                }
            }
            for scroll in [0, 5] {
                app.callers_scroll = scroll;
                for width in [32, 40, 80] {
                    for height in [0, 1, 2, 3, 4, 6] {
                        let area = Rect::new(2, 2, width, height);
                        let mut terminal =
                            Terminal::new(TestBackend::new(width + 4, height + 4)).unwrap();
                        terminal
                            .draw(|f| {
                                for y in 0..height + 4 {
                                    for x in 0..width + 4 {
                                        f.buffer_mut()[(x, y)].set_symbol("~");
                                    }
                                }
                                render_callers(f, area, &app).unwrap();
                            })
                            .unwrap();
                        let buf = terminal.backend().buffer();
                        for y in 0..height + 4 {
                            for x in 0..width + 4 {
                                if !area.contains((x, y).into()) {
                                    assert_eq!(
                                        buf[(x, y)].symbol(),
                                        "~",
                                        "{width}x{height} at {x},{y}"
                                    );
                                }
                            }
                        }
                        if height >= 2 {
                            for x in area.x + 1..area.right() - 1 {
                                assert_eq!(
                                    buf[(x, area.bottom() - 1)].symbol(),
                                    "─",
                                    "bottom border {width}x{height}"
                                );
                            }
                        }
                        if populated && height == 6 {
                            let table_text: String = (area.y + 1..area.bottom() - 2)
                                .flat_map(|y| (area.x + 1..area.right() - 1).map(move |x| (x, y)))
                                .map(|pos| buf[pos].symbol())
                                .collect();
                            assert!(
                                table_text.contains(if scroll == 0 { "K7ABC" } else { "K2ABC" }),
                                "selected caller missing: {table_text}"
                            );
                            assert!(table_text.contains("▶"));
                        }
                        if height >= 3 {
                            let hint: String = (area.x + 1..area.right() - 1)
                                .map(|x| buf[(x, area.bottom() - 2)].symbol())
                                .collect();
                            assert!(
                                hint.contains(if populated { "reply" } else { "(no callers)" }),
                                "{width}x{height}: {hint}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn callers_snr_uses_shared_scale_in_both_themes() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        for theme in [crate::config::Theme::Dark, crate::config::Theme::Light] {
            app.theme = theme;
            for snr in [-15, -11, -10, -1, 0, 5, 10] {
                let mut msg = caller_fixture("K1ABC");
                msg.snr = snr;
                msg.message = "N0CALL K1ABC".into();
                app.decoded_messages.clear();
                app.decoded_messages.push_back(msg);
                let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
                terminal
                    .draw(|f| render_callers(f, f.area(), &app).unwrap())
                    .unwrap();
                let buf = terminal.backend().buffer();
                let label = format!("{snr:+}");
                let row: String = (0..80).map(|x| buf[(x, 2)].symbol()).collect();
                let cells: Vec<char> = row.chars().collect();
                let label_cells: Vec<char> = label.chars().collect();
                let start = cells
                    .windows(label_cells.len())
                    .position(|w| w == label_cells)
                    .unwrap_or_else(|| panic!("missing {label}: {row}"));
                for x in start..start + label.len() {
                    assert_eq!(
                        buf[(x as u16, 2)].fg,
                        super::super::get_snr_color(snr, &app.theme)
                    );
                }
            }
        }
    }

    #[test]
    fn visible_columns_compact_tier_at_operate_80() {
        use CallersColumn::*;
        assert_eq!(visible_columns(32), vec![Call, Msg, Snr]);
    }

    #[test]
    fn visible_columns_at_operate_100() {
        use CallersColumn::*;
        assert_eq!(visible_columns(40), vec![Call, Msg, Snr, Flags]);
    }

    #[test]
    fn visible_columns_at_operate_120() {
        use CallersColumn::*;
        assert_eq!(visible_columns(48), vec![Call, Msg, Snr, Age, Flags]);
    }

    #[test]
    fn visible_columns_wide_tier_shows_all() {
        assert_eq!(visible_columns(53), CallersColumn::ALL.to_vec());
        assert_eq!(visible_columns(80), CallersColumn::ALL.to_vec());
    }

    #[test]
    fn visible_columns_are_in_display_order_and_monotonic() {
        for w in 16..=200u16 {
            let cols = visible_columns(w);
            let positions: Vec<usize> = cols
                .iter()
                .map(|c| CallersColumn::ALL.iter().position(|a| a == c).unwrap())
                .collect();
            assert!(
                positions.windows(2).all(|p| p[0] < p[1]),
                "width {w}: {cols:?} not in display order"
            );
            let wider = visible_columns(w + 1);
            assert!(
                cols.iter().all(|c| wider.contains(c)),
                "width {w}: {cols:?} not a subset of {wider:?}"
            );
        }
    }

    #[test]
    fn visible_columns_always_fit_their_budget() {
        for w in 16..=200u16 {
            let cols = visible_columns(w);
            let used: u16 = cols.iter().map(|c| c.min_width()).sum::<u16>() + cols.len() as u16 - 1;
            assert!(used <= w - 4, "width {w}: {cols:?} needs {used}");
        }
    }

    #[test]
    fn every_header_is_at_least_as_narrow_as_its_column() {
        for c in CallersColumn::ALL {
            assert!(c.header().len() as u16 <= c.min_width(), "{c:?}");
        }
    }

    #[test]
    fn flags_column_fits_two_flags() {
        assert!("BUSY WKD".len() as u16 <= CallersColumn::Flags.min_width());
        assert!("BUSY MINE".len() as u16 <= CallersColumn::Flags.min_width());
    }
}
