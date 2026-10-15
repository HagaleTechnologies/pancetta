//! PAN-89 readiness strip: one always-visible row under the title bar with
//! the TX verdict chip and one chip per active blocker/warning/notice.
//! Redrawn every frame from typed `App` state (see `crate::readiness`), so
//! no status-line write can erase it.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::{App, RigConnDisplay};
use crate::readiness::{readiness_items, verdict, ClockState, ReadinessItem, Severity, Verdict};

/// Chip style for an item of `severity` (shared with the Shift+S panel's
/// readiness dots).
pub fn severity_style(severity: Severity, app: &App) -> Style {
    match severity {
        Severity::Blocker => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
        Severity::Warning => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Severity::Notice => Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    }
}

/// Shift+S dot glyph and colour for a readiness item.
pub fn item_dot(item: &ReadinessItem, app: &App) -> Span<'static> {
    if item.acknowledged {
        return Span::styled("○ ", Style::default().fg(app.theme.muted_color()));
    }
    match item.severity {
        Severity::Blocker => Span::styled("● ", Style::default().fg(Color::Red)),
        Severity::Warning => Span::styled("● ", Style::default().fg(Color::Yellow)),
        Severity::Notice => Span::styled("◌ ", Style::default().fg(Color::Cyan)),
    }
}

fn verdict_style(v: Verdict, app: &App) -> Style {
    let bold = |fg: Color, bg: Color| Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
    match v {
        Verdict::Checking => Style::default().fg(app.theme.muted_color()),
        Verdict::Held => bold(Color::White, Color::Red),
        Verdict::RxOnly => bold(Color::Black, Color::White),
        Verdict::Wait => bold(Color::Black, Color::Cyan),
        Verdict::Open => bold(Color::Black, Color::Yellow),
        Verdict::Ready => bold(Color::Black, Color::Green),
    }
}

/// Muted "all calm" detail after the verdict: `rig ✓ · audio ✓ · clock ✓`.
fn calm_details(app: &App) -> String {
    let mut parts = vec![match app.rig_connected {
        RigConnDisplay::Connected => "rig ✓",
        RigConnDisplay::PollingFailed => "rig ✗",
        // Covers both rig control off (mock rig) and a failed connect, so
        // don't claim either.
        RigConnDisplay::NotConnected => "rig: no CAT",
    }];
    parts.push(match app.pipeline_health.as_ref() {
        None => "audio …",
        Some(h) if h.audio_alive => "audio ✓",
        Some(_) => "audio ✗",
    });
    let skewed = app
        .pipeline_health
        .as_ref()
        .is_some_and(|h| h.clock_skew_warning_s.is_some());
    let clock = app.station_readiness.as_ref().map(|r| r.clock);
    match clock {
        _ if skewed => parts.push("clock ✗"),
        Some(ClockState::Pending | ClockState::Rechecking) => parts.push("clock …"),
        Some(ClockState::InSpec) => parts.push("clock ✓"),
        Some(ClockState::Unreachable) => parts.push("clock ?"),
        Some(ClockState::Skewed) => parts.push("clock ✗"),
        Some(ClockState::NotMonitored) | None => {}
    }
    parts.join(" · ")
}

/// Narrowest truncated chip worth drawing (` ⚠ RIG C… `); with less room
/// the item goes into `+N more · Shift+S` instead.
const MIN_TRUNCATED_CHIP: usize = 10;

/// `label` cut to at most `max` display columns, ending in `…` when cut.
fn truncate_to_width(label: &str, max: usize) -> String {
    if Span::raw(label).width() <= max {
        return label.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in label.chars() {
        let mut buf = [0u8; 4];
        let w = Span::raw(&*ch.encode_utf8(&mut buf)).width();
        if used + w > keep {
            break;
        }
        used += w;
        out.push(ch);
    }
    // Sequences like `⚠️` measure wider as a string than char by char.
    while Span::raw(out.as_str()).width() > keep {
        out.pop();
    }
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

pub fn render_readiness_strip(f: &mut Frame<'_>, area: Rect, app: &App) {
    let width = area.width as usize;
    let items = readiness_items(app);
    let visible: Vec<&ReadinessItem> = items.iter().filter(|i| i.is_visible()).collect();
    let acked = items.len() - visible.len();
    let can_ack = visible.iter().any(|i| i.is_acknowledgeable());
    let v = verdict(app);

    // Right side hints, reserved first so the chips fit around them.
    let mut right: Vec<Span<'static>> = Vec::new();
    if can_ack {
        right.push(Span::styled(
            " ! ack",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ));
    }
    if acked > 0 {
        right.push(Span::styled(
            format!(" {acked} ack'd · Shift+S"),
            Style::default().fg(app.theme.muted_color()),
        ));
    }
    if !right.is_empty() {
        right.push(Span::raw(" "));
    }
    let right_width: usize = right.iter().map(Span::width).sum();
    let budget = width.saturating_sub(right_width);

    let mut left: Vec<Span<'static>> = vec![Span::styled(
        format!(" {} ", v.label()),
        verdict_style(v, app),
    )];
    let mut used: usize = left.iter().map(Span::width).sum();

    if visible.is_empty() {
        let text = if v == Verdict::Checking {
            " waiting for station status".to_string()
        } else {
            format!(" {}", calm_details(app))
        };
        left.push(Span::styled(
            text,
            Style::default().fg(app.theme.muted_color()),
        ));
    } else {
        let more_text = |n: usize| format!(" +{n} more · Shift+S");
        for (idx, item) in visible.iter().enumerate() {
            let chip = Span::styled(
                format!(" {} ", item.label),
                severity_style(item.severity, app),
            );
            let remaining_after = visible.len() - idx - 1;
            let reserve = if remaining_after > 0 {
                Span::raw(more_text(remaining_after)).width()
            } else {
                0
            };
            if used + chip.width() + reserve > budget {
                // Too wide: cut the label to the room left (`⚠ Rig connect
                // failed (conn…`) so a long alert still shows its text;
                // fall back to the counter only when too little is left.
                let room = budget.saturating_sub(used + reserve);
                if room >= MIN_TRUNCATED_CHIP {
                    left.push(Span::styled(
                        format!(" {} ", truncate_to_width(&item.label, room - 2)),
                        severity_style(item.severity, app),
                    ));
                    if remaining_after > 0 {
                        left.push(Span::styled(
                            more_text(remaining_after),
                            Style::default().fg(app.theme.muted_color()),
                        ));
                    }
                    break;
                }
                let more = Span::styled(
                    more_text(visible.len() - idx),
                    Style::default().fg(app.theme.muted_color()),
                );
                if used + more.width() <= budget {
                    left.push(more);
                }
                break;
            }
            used += chip.width();
            left.push(chip);
        }
    }

    let used: usize = left.iter().map(Span::width).sum();
    let padding = width.saturating_sub(used + right_width);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(padding)));
    spans.extend(right);

    let paragraph = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emoji-presentation sequences (`⚠️` = U+26A0 U+FE0F) measure wider as a
    /// string than char by char; the cut must still fit `max`.
    #[test]
    fn truncate_to_width_never_exceeds_max_for_emoji_sequences() {
        let label = "⚠\u{FE0F} rig ⚠\u{FE0F} audio ⚠\u{FE0F} clock ⚠\u{FE0F} input";
        for max in 1..Span::raw(label).width() {
            let cut = truncate_to_width(label, max);
            assert!(Span::raw(cut.as_str()).width() <= max, "{max}: {cut:?}");
        }
    }
}
