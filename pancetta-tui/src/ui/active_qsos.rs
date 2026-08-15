//! Active-QSOs banner — single-row strip rendered above the Band
//! Activity panel. Surfaces in-progress QSOs so the operator never
//! has to switch panels to see who they're mid-conversation with.
//!
//! Data comes from `App::active_qsos`, which the coordinator pushes
//! via `TuiMessage::ActiveQsosUpdate` every time a QSO state changes.
//! The widget is purely a renderer — no derived state, no caching.

use crate::app::App;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

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

    // PAN-21 remediation: `k` (abort) now fires from any panel, including
    // views/zoom states where the QSO Status panel's own selection highlight
    // isn't on screen — Monitor view omits QSO Status entirely, and zooming
    // any OTHER panel fills the whole content area with it. This banner is
    // the one element that's rendered in every view and while zoomed (see
    // `layout_monitor`/the zoom branch in `draw()`), so when there's more
    // than one active QSO (the only case where "which one?" is actually
    // ambiguous) it marks the pinned/selected one — same "▶" + reversed-video
    // convention `qso_status::render_multi_qso_table` already uses — so the
    // abort target stays visible no matter what the operator is looking at.
    let selected_id = app.selected_qso_id();
    let mark_selection = qsos.len() > 1;

    // Round-2 remediation: the render loop below stops once the row's width
    // budget runs out, so on a narrow terminal or a large pileup the
    // newest-first sort could place the selected/pinned QSO past the point
    // where anything more still fits — silently dropping it (and its "▶"
    // marker) off the visible slice entirely, even though `k` still targets
    // it. The loop's very first entry is always rendered unconditionally
    // (the budget check below is gated on `shown > 0`), so guarantee the
    // selected QSO occupies that slot whenever there's more than one QSO —
    // it can never be the one truncated. This deliberately trades strict
    // newest-first ordering for "the abort target is always visible," which
    // is the property that actually matters here.
    if mark_selection {
        if let Some(id) = selected_id.as_deref() {
            if let Some(pos) = qsos.iter().position(|q| q.qso_id == id) {
                if pos != 0 {
                    let selected = qsos.remove(pos);
                    qsos.insert(0, selected);
                }
            }
        }
    }

    let now = chrono::Utc::now();
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        "QSO: ",
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));

    let budget = area.width as usize;
    let mut used: usize = spans.iter().map(Span::width).sum();
    let mut shown = 0usize;
    for (idx, q) in qsos.iter().enumerate() {
        let elapsed = (now - q.started_at).num_seconds().max(0);
        let mm = elapsed / 60;
        let ss = elapsed % 60;
        let separator = if idx > 0 { "  │  " } else { "" };
        let is_selected = mark_selection && selected_id.as_deref() == Some(q.qso_id.as_str());
        let marker = if is_selected { "▶" } else { "" };
        let detail = format!(
            " ({} · {}:{:02} · {:.0}Hz)",
            friendly_state(&q.state),
            mm,
            ss,
            q.frequency_hz
        );
        let remaining = qsos.len() - idx - 1;
        let tail_width = if remaining > 0 {
            format!("  │  +{remaining} more").chars().count()
        } else {
            0
        };
        if shown > 0
            && used
                + separator.chars().count()
                + marker.chars().count()
                + q.their_callsign.chars().count()
                + detail.chars().count()
                + tail_width
                > budget
        {
            break;
        }
        if idx > 0 {
            spans.push(Span::styled(
                separator,
                Style::default().fg(app.theme.muted_color()),
            ));
        }
        if is_selected {
            spans.push(Span::styled(
                marker,
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let mut call_style = Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD);
        if is_selected {
            call_style = call_style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(q.their_callsign.clone(), call_style));
        spans.push(Span::styled(
            detail.clone(),
            Style::default().fg(app.theme.foreground_color()),
        ));
        used += separator.chars().count()
            + marker.chars().count()
            + q.their_callsign.chars().count()
            + detail.chars().count();
        shown += 1;
    }
    if shown < qsos.len() {
        spans.push(Span::styled(
            format!("  │  +{} more", qsos.len() - shown),
            Style::default().fg(app.theme.muted_color()),
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

#[cfg(test)]
mod tests {
    //! PAN-21 round-1 remediation (Codex P1): with `k` now aborting the
    //! selected QSO from any panel — including Monitor view and zoom on a
    //! non-QSO-Status panel, neither of which render the QSO Status table's
    //! own selection highlight — this banner is the one element rendered in
    //! every view/zoom state, so it must mark which QSO `k` would hit
    //! whenever there's more than one active QSO to disambiguate between.
    use super::*;
    use crate::app::App;
    use ratatui::{backend::TestBackend, Terminal};

    fn banner(call: &str, qso_id: &str, started_secs_ago: i64) -> crate::app::ActiveQsoBanner {
        crate::app::ActiveQsoBanner {
            their_callsign: call.to_string(),
            state: "wait rpt".to_string(),
            started_at: chrono::Utc::now() - chrono::Duration::seconds(started_secs_ago),
            frequency_hz: 1500.0,
            tx_parity: None,
            last_tx_text: None,
            last_tx_at: None,
            last_rx_text: None,
            last_rx_at: None,
            snr_rx: None,
            report_sent: None,
            report_received: None,
            exchange_count: 0,
            qso_id: qso_id.to_string(),
            initiated_by: "Manual".to_string(),
            ladder_labels: vec![],
            ladder_ours: vec![],
            ladder_index: 0,
            now_line: String::new(),
            next_line: String::new(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
            dx_last_activity: None,
            hound: false,
        }
    }

    fn render(app: &App) -> String {
        render_at_width(app, 100)
    }

    fn render_at_width(app: &App, width: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, 1)).unwrap();
        term.draw(|f| render_active_qsos(f, f.area(), app)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// With exactly one active QSO there is no ambiguity about what `k`
    /// would hit, so the marker stays off — matches the multi-QSO table's
    /// own `active_count > 1` gate in `qso_status.rs`.
    #[tokio::test]
    async fn single_active_qso_shows_no_selection_marker() {
        let mut app = App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.apply_active_qsos(vec![banner("W1AW", "qso-1", 10)], Vec::new());
        let rendered = render(&app);
        assert!(rendered.contains("W1AW"));
        assert!(
            !rendered.contains('▶'),
            "single QSO is unambiguous, no marker needed: {rendered}"
        );
    }

    /// With multiple active QSOs, the banner marks whichever one is
    /// currently pinned/selected (`App::selected_qso_id`) — the same QSO
    /// `k` would abort — so the target is visible even where the QSO
    /// Status table itself isn't on screen (Monitor view, zoom). The banner
    /// displays newest-first (`b.started_at.cmp(&a.started_at)`), which is
    /// independent of `apply_active_qsos`'s storage/pin order, so this
    /// deliberately puts the pinned QSO (K5ARH, pinned because it's index 0
    /// of the vec passed in) SECOND in display order (W1AW is more recent)
    /// to prove the marker tracks the pin, not display position.
    #[tokio::test]
    async fn multiple_active_qsos_mark_the_selected_one() {
        let mut app = App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.apply_active_qsos(
            vec![
                banner("K5ARH", "qso-k5arh", 30),
                banner("W1AW", "qso-w1aw", 0),
            ],
            Vec::new(),
        );
        // apply_active_qsos pins the qso at qso_cursor (0 = the first entry
        // passed in, K5ARH) when nothing was pinned yet.
        assert_eq!(app.selected_qso_callsign().as_deref(), Some("K5ARH"));

        let rendered = render(&app);
        assert!(
            rendered.contains("▶K5ARH"),
            "marker must sit right before the selected (pinned) callsign: {rendered}"
        );
        assert!(
            !rendered.contains("▶W1AW"),
            "unselected QSO must not carry the marker: {rendered}"
        );

        // Move the pin to the other QSO and confirm the marker follows it.
        app.qso_cursor_down();
        assert_eq!(app.selected_qso_callsign().as_deref(), Some("W1AW"));
        let rendered2 = render(&app);
        assert!(
            rendered2.contains("▶W1AW"),
            "marker should have followed the pin to W1AW: {rendered2}"
        );
        assert!(!rendered2.contains("▶K5ARH"));
    }

    /// Round-2 remediation (Codex P1): on a narrow terminal (or a large
    /// pileup), the render loop's width budget can run out before it
    /// reaches the selected/pinned QSO in newest-first order — the old
    /// code would silently drop it (and its marker) off the visible slice.
    /// Pin the OLDEST of several QSOs (so it would sort dead last) and
    /// render into an area that only fits a single entry: the selected one
    /// must still be the one shown, marked, not truncated away.
    #[tokio::test]
    async fn selected_qso_is_never_truncated_off_a_narrow_banner() {
        let mut app = App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        // Newest-first display order would be: NEWEST, MID, OLDEST — so
        // OLDEST is the one that overflow truncation would drop first.
        app.apply_active_qsos(
            vec![
                banner("OLDEST", "qso-oldest", 120),
                banner("MID", "qso-mid", 60),
                banner("NEWEST", "qso-newest", 0),
            ],
            Vec::new(),
        );
        // Pin OLDEST (index 0 of the vec passed to apply_active_qsos).
        assert_eq!(app.selected_qso_callsign().as_deref(), Some("OLDEST"));

        // Wide enough for "QSO: " plus exactly one entry, not two.
        let rendered = render_at_width(&app, 45);
        assert!(
            rendered.contains("OLDEST"),
            "selected QSO must never be truncated off the banner: {rendered:?}"
        );
        assert!(
            rendered.contains("▶OLDEST"),
            "selected QSO must still carry its marker when forced into view: {rendered:?}"
        );
        assert!(
            !rendered.contains("NEWEST") && !rendered.contains("MID"),
            "sanity check: the area really is too narrow for more than one entry: {rendered:?}"
        );
    }

    /// The fix must actually show up in Monitor view specifically — the
    /// view Codex's finding called out as omitting QSO Status entirely.
    #[tokio::test]
    async fn monitor_view_banner_marks_the_selected_qso_when_multiple_are_active() {
        let mut app = App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.apply_active_qsos(
            vec![
                banner("K5ARH", "qso-k5arh", 30),
                banner("W1AW", "qso-w1aw", 0),
            ],
            Vec::new(),
        );
        app.active_view = crate::view::ActiveView::Monitor;

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer();
        let row0: String = (0..buf.area.width)
            .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
            .collect();
        // Row 1 is the active-QSO banner (row 0 is the title bar) in Monitor
        // view's layout (`compute_monitor_rects`).
        assert!(
            row0.contains('▶'),
            "Monitor view's banner must mark the selected QSO: {row0}"
        );
    }
}
