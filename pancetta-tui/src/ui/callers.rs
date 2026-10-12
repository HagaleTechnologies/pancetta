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

    // PAN-88: narrow panels show fewer, whole columns. `table_area` below
    // has the same width as `area`.
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
        .block(block)
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
