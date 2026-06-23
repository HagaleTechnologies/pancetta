//! Active-QSOs banner — single-row strip rendered above the Band
//! Activity panel. Surfaces in-progress QSOs so the operator never
//! has to switch panels to see who they're mid-conversation with.
//!
//! Data comes from `App::active_qsos`, which the coordinator pushes
//! via `TuiMessage::ActiveQsosUpdate` every time a QSO state changes.
//! The widget is purely a renderer — no derived state, no caching.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;

/// Render a one-line banner summarising active QSOs into `area`.
/// Empty list renders as a muted "QSO: (none)" placeholder so the
/// banner row always has consistent visual weight.
pub fn render_active_qsos(f: &mut Frame<'_>, area: Rect, app: &App) {
    if app.active_qsos.is_empty() {
        let text = Line::from(Span::styled(
            "QSO: (none in progress)",
            Style::default().fg(app.theme.muted_color()),
        ));
        f.render_widget(Paragraph::new(text), area);
        return;
    }

    // Newest QSO first — operator most-recently engaged is most relevant.
    let mut qsos = app.active_qsos.clone();
    qsos.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    let now = chrono::Utc::now();
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        "QSO: ",
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));

    for (idx, q) in qsos.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(app.theme.muted_color()),
            ));
        }
        let elapsed = (now - q.started_at).num_seconds().max(0);
        let mm = elapsed / 60;
        let ss = elapsed % 60;
        spans.push(Span::styled(
            q.their_callsign.clone(),
            Style::default()
                .fg(app.theme.success_color())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(
                " ({} · {}:{:02} · {:.0}Hz)",
                friendly_state(&q.state),
                mm,
                ss,
                q.frequency_hz
            ),
            Style::default().fg(app.theme.foreground_color()),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compress verbose QSO state names into something readable in a single
/// banner row. Maps known state strings; falls back to the raw value
/// for anything we haven't enumerated yet.
fn friendly_state(state: &str) -> &str {
    match state {
        "RespondingToCq" => "→ called",
        "WaitingForReport" => "wait rpt",
        "SendingReport" => "sending rpt",
        "WaitingForConfirmation" => "wait RR73",
        "SendingConfirmation" => "sending RR73",
        "Sending73" => "sending 73",
        "Completed" => "done",
        other => other,
    }
}
