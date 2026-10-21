//! Scrollable explanation for the station selected in any panel.
use crate::app::App;
use crate::dx_reason::{self, FocusedReason};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub fn render_why_overlay(f: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let modal = Rect::new(area.x + 2, area.y + 2, area.width - 4, area.height - 4);
    let now = chrono::Utc::now();
    let (title, lines) = match dx_reason::focused(app, now) {
        FocusedReason::NoFocus => (
            " Why? — Esc/w close ".to_owned(),
            vec![
                "No station focused. Tab to a list, pick a station with ↑/↓, then press w."
                    .to_owned(),
            ],
        ),
        FocusedReason::NotRanked(call) => (
            format!(" Why {call}? — ↑↓ scroll, Esc/w close "),
            vec![format!(
                "{call} is not ranked now: not heard or spotted in the last 3 min."
            )],
        ),
        FocusedReason::Ranked(call, station) => {
            let lines =
                dx_reason::detail_for(station, app.dx_need_for(&station.call_sign), now).lines;
            (format!(" Why {call}? — ↑↓ scroll, Esc/w close "), lines)
        }
    };
    f.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    let wrapped: Vec<Line<'static>> = lines
        .iter()
        .flat_map(|line| wrap_line(line, inner.width as usize))
        .map(Line::from)
        .collect();
    let max_scroll = wrapped
        .len()
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.why_scroll = app.why_scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(wrapped)
            .scroll((app.why_scroll, 0))
            .style(Style::default().fg(app.theme.foreground_color())),
        inner,
    );
}

/// Preserve short arithmetic rows verbatim; wrap prose at word boundaries.
/// Long individual tokens are split only when they cannot fit even alone.
fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    if Span::raw(text).width() <= width {
        return vec![text.to_owned()];
    }
    let indent = " ".repeat(5.min(width.saturating_sub(1)));
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!current.trim().is_empty());
        if Span::raw(&current).width() + separator + Span::raw(word).width() > width
            && !current.trim().is_empty()
        {
            lines.push(current);
            current = indent.clone();
        }
        if !current.trim().is_empty() {
            current.push(' ');
        }
        for ch in word.chars() {
            if Span::raw(&current).width() + Span::raw(ch.to_string()).width() > width {
                lines.push(current);
                current = indent.clone();
            }
            current.push(ch);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| render_why_overlay(f, f.area(), app))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn why_overlay_empty_states() {
        let mut app = App::new(crate::Config::default(), None).await.unwrap();
        assert!(render(&mut app, 100, 30).contains("No station focused. Tab to a list"));
        assert!(render(&mut app, 19, 5).trim().is_empty());
        let mut app = super::super::station_card::tests::ranked_app("JA1ABC", true, false).await;
        app.dx_stations.get_mut("JA1ABC").unwrap().last_seen -= chrono::Duration::minutes(4);
        assert!(render(&mut app, 132, 40)
            .contains("JA1ABC is not ranked now: not heard or spotted in the last 3 min."));
        let removed = app.dx_stations.remove("JA1ABC").unwrap();
        assert!(render(&mut app, 132, 40)
            .contains("JA1ABC is not ranked now: not heard or spotted in the last 3 min."));
        app.dx_stations.insert("JA1ABC".into(), removed);
        let station = app.dx_stations.get_mut("JA1ABC").unwrap();
        station.last_seen = chrono::Utc::now();
        station.local_rank = None;
        station.network_rank = None;
        assert!(render(&mut app, 132, 40).contains("JA1ABC has not been scored yet: it came from the DX cluster with no decode or cqdx data."));
    }

    #[tokio::test]
    async fn why_overlay_renders_sections() {
        let mut app = super::super::station_card::tests::ranked_app("K1ABC/P", false, true).await;
        let output = render(&mut app, 132, 40);
        assert!(output.contains("Why K1ABC/P?"));
        assert!(
            output.contains("Pri 1202: no-award-need tier (1000–1999), 202 of 999 within the tier"),
            "{output}"
        );
        assert!(output.contains("Portable suffix: maybe POTA/SOTA (/P)"));
        assert!(output.contains("not a confirmed park or summit activation"));
        assert!(output
            .lines()
            .any(|line| line.contains("QSO failed this session") && line.contains("yes")));
        assert!(output.contains("(+0.081 ÷ 0.40 ceiling) × 1.00 freshness = 0.202 → 202 of 999"));
        assert!(output.contains("Within-tier score"));
        assert!(output.contains(
            "Scored by the station from your log and this session's history (local decode)."
        ));
    }

    #[tokio::test]
    async fn why_overlay_scrolls_when_taller_than_the_modal() {
        let mut app = super::super::station_card::tests::ranked_app("K1ABC/P", false, true).await;
        let first = render(&mut app, 80, 24);
        assert!(!first.contains("Scored by the station"));
        app.why_scroll = u16::MAX;
        let last = render(&mut app, 80, 24);
        assert!(last.contains("Scored by the station"), "{last}");
        assert!(app.why_scroll > 0 && app.why_scroll < u16::MAX);
        let clamped = app.why_scroll;
        render(&mut app, 80, 24);
        assert_eq!(app.why_scroll, clamped);
        render(&mut app, 132, 60);
        assert_eq!(app.why_scroll, 0);
    }

    #[test]
    fn wrapping_preserves_words_and_uses_hanging_indent_without_clipping() {
        let original = "Why: an explanation that needs more than one line with Unicode → text";
        let lines = wrap_line(original, 24);
        assert!(lines.iter().all(|line| Span::raw(line).width() <= 24));
        assert!(lines.iter().skip(1).all(|line| line.starts_with("     ")));
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            original.split_whitespace().collect::<Vec<_>>()
        );
        assert!(wrap_line("a_really_long_unbroken_token", 14)
            .iter()
            .all(|line| Span::raw(line).width() <= 14));
    }
}
