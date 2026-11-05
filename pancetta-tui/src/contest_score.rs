//! Formatting and geometry only. Scoring is owned by pancetta-qso.
use pancetta_qso::contest::ledger::{ContestScoreState, ContestScoreUpdate};
use ratatui::{layout::Rect, widgets::Paragraph, Frame};

pub fn lines(update: &ContestScoreUpdate, width: u16) -> Vec<String> {
    let value = match &update.state {
        ContestScoreState::Disabled => return vec![],
        ContestScoreState::Loading => "Loading contest history...".into(),
        ContestScoreState::Unavailable(reason) => format!("Score unavailable: {reason}"),
        ContestScoreState::Ready(s) => {
            format!("{}{}", s.score, if s.partial { " (partial)" } else { "" })
        }
    };
    if width < 60 {
        return vec![format!("Contest: {value}")];
    }
    let mut result = match &update.state {
        ContestScoreState::Ready(s) => {
            let mult = if s.multipliers_available {
                s.multiplier_count.to_string()
            } else {
                "n/a".into()
            };
            // Shorten the name first, then omit rate, always retaining score.
            let metrics = format!("Score: {value} | Mult: {mult}");
            let rate = format!(" | Rate (60m): {}/h", s.rate_per_hour);
            let suffix = format!(" | {metrics}{rate}");
            let first = if 8 + suffix.len() <= width as usize {
                let available = (width as usize).saturating_sub(8 + suffix.len());
                let name: String = update.display_name.chars().take(available).collect();
                format!("Contest {name}{suffix}")
            } else {
                format!("Contest | {metrics}")
            };
            vec![
                first,
                format!(
                    "Scored: {} | Dupes: {} | Pending: {} | Excluded: {}",
                    s.scored, s.dupes, s.pending, s.excluded
                ),
            ]
        }
        _ => vec![format!("Contest {} | {value}", update.display_name)],
    };
    if update.outside_period {
        result[0].push_str(" | Outside contest period");
    }
    result
}

pub fn content(area: Rect, update: Option<&ContestScoreUpdate>) -> Rect {
    let rows = update
        .map(|s| lines(s, area.width).len() as u16)
        .unwrap_or(0)
        .min(area.height.saturating_sub(1));
    Rect {
        y: area.y + rows,
        height: area.height - rows,
        ..area
    }
}

pub fn render(f: &mut Frame<'_>, area: Rect, update: Option<&ContestScoreUpdate>) {
    let Some(update) = update else {
        return;
    };
    let body = content(area, Some(update));
    let strip = Rect {
        height: body.y - area.y,
        ..area
    };
    f.render_widget(Paragraph::new(lines(update, area.width).join("\n")), strip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_qso::contest::scoring::*;
    fn update(state: ContestScoreState) -> ContestScoreUpdate {
        ContestScoreUpdate {
            generation: 2,
            instance_id: "ks".into(),
            rules_id: KSQP_FT_2026.into(),
            display_name: "KSQP 2026".into(),
            outside_period: false,
            state,
        }
    }
    #[test]
    fn loading_error_disabled_and_narrow_have_exact_words() {
        assert_eq!(
            lines(&update(ContestScoreState::Loading), 100),
            ["Contest KSQP 2026 | Loading contest history..."]
        );
        assert_eq!(
            lines(
                &update(ContestScoreState::Unavailable(
                    "contest history could not be read".into()
                )),
                100
            ),
            ["Contest KSQP 2026 | Score unavailable: contest history could not be read"]
        );
        assert!(lines(&update(ContestScoreState::Disabled), 100).is_empty());
        assert_eq!(
            lines(&update(ContestScoreState::Loading), 50),
            ["Contest: Loading contest history..."]
        );
    }
    #[test]
    fn render_empty_score_and_preserve_content_geometry() {
        let context =
            ContestScoringContext::new("ks", "us-state-qso-party", KSQP_FT_2026, "TX", vec![])
                .unwrap();
        let snapshot = reduce_contest_score(&context, &[], chrono::Utc::now()).unwrap();
        let u = update(ContestScoreState::Ready(snapshot));
        let backend = ratatui::backend::TestBackend::new(100, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), Some(&u))).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Contest KSQP 2026 | Score: 0 | Mult: n/a | Rate (60m): 0/h"));
        assert!(text.contains("Scored: 0 | Dupes: 0 | Pending: 0 | Excluded: 0"));
        assert_eq!(
            content(Rect::new(0, 0, 100, 5), Some(&u)),
            Rect::new(0, 2, 100, 3)
        );
        assert_eq!(
            content(Rect::new(0, 0, 50, 5), Some(&u)),
            Rect::new(0, 1, 50, 4)
        );
    }
    #[test]
    fn partial_sc_and_outside_period_render_exact_metrics() {
        let snapshot = ContestScoreSnapshot {
            scored: 3,
            dupes: 1,
            pending: 2,
            excluded: 0,
            multiplier_count: 2,
            multipliers_available: true,
            score: 12,
            partial: true,
            rate_per_hour: 3,
            per_contact: vec![],
        };
        let mut u = update(ContestScoreState::Ready(snapshot));
        u.display_name = "SCQP 2026".into();
        assert_eq!(
            lines(&u, 100),
            [
                "Contest SCQP 2026 | Score: 12 (partial) | Mult: 2 | Rate (60m): 3/h",
                "Scored: 3 | Dupes: 1 | Pending: 2 | Excluded: 0",
            ]
        );
        u.outside_period = true;
        assert!(lines(&u, 120)[0].ends_with(" | Outside contest period"));
        assert_eq!(lines(&u, 40), ["Contest: 12 (partial)"]);
        let area = Rect::new(0, 2, 100, 15);
        u.state = ContestScoreState::Unavailable("contest rules are unsupported".into());
        assert_eq!(content(area, Some(&u)).height, 14);
        u.state = ContestScoreState::Disabled;
        assert_eq!(content(area, Some(&u)), area);
    }
}
