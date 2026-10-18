//! The focused station's card has four content lines: callsign and status,
//! ranking reason, grid/distance/last-heard message, and QSO or call action.
//! Hunt view reserves all four lines; Operate clips to its available height.
//!
//! **Documented spec divergence:** the original design (spec §4) sources
//! "what they're doing right now" from the coordinator's `DxActivityMap`.
//! That map only reaches the TUI for ACTIVE QSOs
//! (`ActiveQsoSnapshotItem.dx_last_activity`). This renderer instead derives
//! activity from the newest entry in `App.decoded_messages` for the focused
//! call — equivalent information for any on-band station the operator can
//! focus, with no new bus wiring. Threading full `DxActivityMap` summaries
//! for non-engaged stations to the TUI is an additive follow-up if this
//! proves too thin.

use anyhow::Result;
use pancetta_core::callsign::callsigns_match;
use pancetta_core::slot::SlotParity;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::{create_panel_block, format_distance, format_time_ago};
use crate::app::{App, DecodedMessageView};

/// Render four content lines: call/entity/status, ranking reason,
/// grid/distance/last-message, and engagement or transmit window;
/// one fallback line when nothing is focused.
pub fn render_station_card(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let block = create_panel_block("Station", false, app);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(call) = app.focused_callsign() else {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "(no station focused — Tab to a list, ↑/↓ to pick)",
            Style::default().fg(app.theme.muted_color()),
        )));
        f.render_widget(paragraph, inner);
        return Ok(());
    };

    // Newest decode from the focused station drives activity lines (see module
    // doc for the DxActivityMap divergence).
    let last_decode = newest_decode_for(app, &call);

    let line1 = render_line1(app, &call, last_decode);
    let line2 = render_line2(app, last_decode);
    let line3 = render_line3(app, &call, last_decode);

    let why = render_why_line(app, inner.width, chrono::Utc::now());
    let paragraph = Paragraph::new(vec![line1, why, line2, line3]);
    f.render_widget(paragraph, inner);

    Ok(())
}

/// Fit complete clauses so clipping cannot hide a caution or a hedge.
fn render_why_line(app: &App, width: u16, now: chrono::DateTime<chrono::Utc>) -> Line<'static> {
    use crate::dx_reason::{self, FocusedReason};
    let (clauses, caution_count) = match dx_reason::focused(app, now) {
        FocusedReason::NoFocus => return Line::default(),
        FocusedReason::NotRanked(_) => (
            vec!["not ranked now (not heard or spotted in the last 3 min)".into()],
            0,
        ),
        FocusedReason::Ranked(_, station) => {
            let reason = dx_reason::reason_for(station, app.dx_need_for(&station.call_sign), now);
            (reason.summary_clauses(), reason.cautions.len())
        }
    };
    let fitted = dx_reason::fit_clauses(&clauses, width.saturating_sub(5) as usize);
    let mut spans = vec![Span::styled(
        "Why: ",
        Style::default().fg(app.theme.muted_color()),
    )];
    for (index, clause) in fitted.split("; ").enumerate() {
        if index > 0 {
            spans.push(Span::raw("; "));
        }
        let style = if clause == "…" {
            Style::default().fg(app.theme.foreground_color())
        } else if index == 0 {
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD)
        } else if index <= caution_count {
            Style::default().fg(app.theme.warning_color())
        } else {
            Style::default().fg(app.theme.foreground_color())
        };
        spans.push(Span::styled(clause.to_owned(), style));
    }
    Line::from(spans)
}

/// The most recently decoded message from `call` (compound-aware match),
/// walking `decoded_messages` newest-first.
fn newest_decode_for<'a>(app: &'a App, call: &str) -> Option<&'a DecodedMessageView> {
    app.decoded_messages.iter().rev().find(|m| {
        m.call_sign
            .as_deref()
            .is_some_and(|c| callsigns_match(c, call))
    })
}

/// Line 1: `"{call} — {entity|'---'} {ATNO★|needed|worked|new}"`.
fn render_line1<'a>(app: &App, call: &str, last_decode: Option<&DecodedMessageView>) -> Line<'a> {
    // PAN-85: append the station's US state/territory when known, matching
    // DX Hunter. Deliberately no offline `dxcc::entity_for_callsign`
    // fallback here — out of scope (see D10).
    let entity = app
        .dx_stations
        .get(call)
        .and_then(|d| d.entity_name.clone())
        .map(|e| crate::dxcc::format_entity_with_state(&e, app.station_state_for(call)))
        .unwrap_or_else(|| "---".to_string());

    let status_flag = last_decode
        .map(|m| {
            if m.atno {
                "ATNO\u{2605}"
            } else if m.needed {
                "needed"
            } else if m.worked_before {
                "worked"
            } else {
                "new"
            }
        })
        .unwrap_or("new");

    Line::from(vec![
        Span::styled(
            call.to_string(),
            Style::default()
                .fg(app.theme.success_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" \u{2014} "),
        Span::styled(entity, Style::default().fg(app.theme.foreground_color())),
        Span::raw(" "),
        Span::styled(status_flag, Style::default().fg(app.theme.accent_color())),
    ])
}

/// Third content line: `"{grid} {dist}km · {last-msg-text} ({snr:+}) {age}"`.
fn render_line2<'a>(app: &App, last_decode: Option<&DecodedMessageView>) -> Line<'a> {
    let grid = last_decode
        .and_then(|m| m.grid_square.clone())
        .unwrap_or_else(|| "----".to_string());
    let dist_str = format_distance(last_decode.and_then(|m| m.distance));
    let msg_text = last_decode.map(|m| m.message.clone()).unwrap_or_default();
    let snr_str = last_decode
        .map(|m| format!("{:+}", m.snr))
        .unwrap_or_else(|| "---".to_string());
    let age = last_decode
        .map(|m| format_time_ago(m.timestamp))
        .unwrap_or_else(|| "---".to_string());

    Line::from(Span::styled(
        format!("{grid} {dist_str} \u{b7} {msg_text} ({snr_str}) {age}"),
        Style::default().fg(app.theme.foreground_color()),
    ))
}

/// Fourth content line: engaged → `"in QSO: {state}"`; else `"{E|O}-window · Space=call"`
/// (window = newest decode's `slot_parity`).
fn render_line3<'a>(app: &App, call: &str, last_decode: Option<&DecodedMessageView>) -> Line<'a> {
    let engaged_state = app
        .active_qsos
        .iter()
        .find(|q| callsigns_match(&q.their_callsign, call))
        .map(|q| q.state.clone());

    let text = if let Some(state) = engaged_state {
        format!("in QSO: {state}")
    } else {
        let window = match last_decode.and_then(|m| m.slot_parity) {
            Some(SlotParity::Even) => "E",
            Some(SlotParity::Odd) => "O",
            None => "-",
        };
        format!("{window}-window \u{b7} Space=call")
    };

    Line::from(Span::styled(
        text,
        Style::default().fg(app.theme.muted_color()),
    ))
}

#[cfg(test)]
pub(in crate::ui) mod tests {
    use super::*;
    use crate::app::{ActivePanel, ActiveQsoBanner, App};
    use crate::config::Config;
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture_view(call: &str, snr: i32) -> DecodedMessageView {
        DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14.074,
            mode: "FT8".to_string(),
            snr,
            delta_time: 0.0,
            delta_freq: 1500.0,
            call_sign: Some(call.to_string()),
            grid_square: Some("FN42".to_string()),
            message: format!("CQ {call} FN42"),
            distance: Some(1234.0),
            bearing: None,
            slot_parity: Some(SlotParity::Even),
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

    fn fixture_banner(call: &str, state: &str) -> ActiveQsoBanner {
        ActiveQsoBanner {
            their_callsign: call.to_string(),
            state: state.to_string(),
            started_at: chrono::Utc::now(),
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
            qso_id: format!("{call}-id"),
            initiated_by: "Manual".to_string(),
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

    struct RankingLookup {
        band_new: bool,
        failed: bool,
    }
    impl pancetta_qso::priority::WorkedStationLookup for RankingLookup {
        fn is_duplicate(&self, _: &str, _: f64) -> bool {
            false
        }
        fn is_recent_failure(&self, _: &str) -> bool {
            self.failed
        }
        fn is_needed_dxcc(&self, _: &str) -> bool {
            false
        }
        fn is_needed_grid(&self, _: &str) -> bool {
            false
        }
        fn is_dxcc_needed_on_band(&self, _: &str, _: f64) -> bool {
            self.band_new
        }
    }

    pub(in crate::ui) async fn ranked_app(call: &str, band_new: bool, failed: bool) -> App {
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        let mut app = fixture_app().await;
        let mut view = fixture_view(call, -3);
        let explained = PriorityScorer::new(
            PriorityWeights::default(),
            Box::new(RankingLookup { band_new, failed }),
        )
        .score_tiered_explained(call, Some("FN42"), -3, 14_074_000.0);
        view.priority_score = Some(explained.score.as_display_u32());
        view.priority_factors = Some(explained.factors);
        view.band_needed = band_new;
        app.add_decoded_message(view).await.unwrap();
        app.active_panel = ActivePanel::DxHunter;
        app
    }

    async fn fixture_app() -> App {
        App::new(Config::default(), None).await.unwrap()
    }

    fn render(app: &App) -> ratatui::buffer::Buffer {
        render_at(app, 60, 10)
    }

    fn render_at(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_station_card(f, area, app).unwrap();
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.contains(needle)
        })
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[tokio::test]
    async fn focused_ranked_station_shows_why_line_second() {
        let app = ranked_app("JA1ABC", true, false).await;
        let buf = render_at(&app, 130, 6);
        assert!(row(&buf, 2).contains("Why: new entity on 20M"));
        assert!(row(&buf, 3).contains("CQ JA1ABC FN42"));
        assert!(row(&buf, 4).contains("E-window · Space=call"));
        let why = row(&buf, 2);
        let col = why[..why.find("new entity").unwrap()].chars().count() as u16;
        assert_eq!(buf[(col, 2)].fg, app.theme.accent_color());
        assert!(buf[(col, 2)].modifier.contains(Modifier::BOLD));
    }

    #[tokio::test]
    async fn why_line_states() {
        let mut app = fixture_app().await;
        assert!(!buffer_contains(&render_at(&app, 130, 6), "Why:"));
        app.decoded_messages.push_back(fixture_view("JA1ABC", -7));
        assert!(buffer_contains(
            &render_at(&app, 130, 6),
            "Why: not ranked now (not heard or spotted in the last 3 min)"
        ));
        let mut app = ranked_app("JA1ABC", true, false).await;
        app.dx_stations.get_mut("JA1ABC").unwrap().last_seen -= chrono::Duration::minutes(4);
        assert!(buffer_contains(
            &render_at(&app, 130, 6),
            "Why: not ranked now (not heard or spotted in the last 3 min)"
        ));
        let station = app.dx_stations.get_mut("JA1ABC").unwrap();
        station.last_seen = chrono::Utc::now();
        station.local_rank = None;
        station.network_rank = None;
        assert!(buffer_contains(
            &render_at(&app, 130, 6),
            "Why: not scored yet (no decode or cqdx data)"
        ));
    }

    #[tokio::test]
    async fn why_line_fits_whole_clauses() {
        let app = ranked_app("K1ABC/P", false, true).await;
        let buf = render_at(&app, 53, 6);
        assert!(
            row(&buf, 2).contains("Why: no award need; QSO failed this session; …"),
            "{}",
            row(&buf, 2)
        );
        let text = row(&buf, 2);
        let col = text[..text.find("QSO failed").unwrap()].chars().count() as u16;
        assert_eq!(buf[(col, 2)].fg, app.theme.warning_color());
    }

    #[tokio::test]
    async fn why_line_follows_the_pinned_station_across_a_resort() {
        let mut app = ranked_app("JA1ABC", true, false).await;
        app.clamp_dx_hunter_selection();
        assert!(buffer_contains(
            &render_at(&app, 130, 6),
            "Why: new entity on 20M"
        ));
        let mut view = fixture_view("VK9DX", -5);
        view.priority_score = Some(5999);
        app.add_decoded_message(view).await.unwrap();
        assert_eq!(app.focused_callsign().as_deref(), Some("JA1ABC"));
        assert!(buffer_contains(
            &render_at(&app, 130, 6),
            "Why: new entity on 20M"
        ));
    }

    #[tokio::test]
    async fn portable_station_card_hedges() {
        let app = ranked_app("W5XYZ/P", false, false).await;
        let buf = render_at(&app, 130, 6);
        assert!(buffer_contains(&buf, "maybe POTA/SOTA (/P suffix)"));
        assert!(!buffer_contains(&buf, "activation"));
    }

    #[tokio::test]
    async fn focused_atno_station_shows_call_and_atno() {
        let mut app = fixture_app().await;
        assert_eq!(app.active_panel, ActivePanel::BandActivity);
        let mut view = fixture_view("K1AAA", -10);
        view.atno = true;
        app.decoded_messages.push_back(view);

        // Default band_activity_scroll (0) + no pin resolves the focus to
        // the only decoded message via `get_selected_station`.
        assert_eq!(app.focused_callsign(), Some("K1AAA".to_string()));

        let buf = render(&app);
        assert!(buffer_contains(&buf, "K1AAA"), "missing callsign");
        assert!(buffer_contains(&buf, "ATNO"), "missing ATNO flag");
    }

    #[tokio::test]
    async fn no_focus_shows_fallback_message() {
        let app = fixture_app().await;
        assert_eq!(app.focused_callsign(), None);

        let buf = render(&app);
        assert!(
            buffer_contains(&buf, "no station focused"),
            "missing fallback message"
        );
    }

    #[tokio::test]
    async fn needed_worked_and_new_flags_render() {
        for (atno, needed, worked_before, expected) in [
            (false, true, false, "needed"),
            (false, false, true, "worked"),
            (false, false, false, "new"),
        ] {
            let mut app = fixture_app().await;
            let mut view = fixture_view("W1XYZ", -5);
            view.atno = atno;
            view.needed = needed;
            view.worked_before = worked_before;
            app.decoded_messages.push_back(view);

            let buf = render(&app);
            assert!(
                buffer_contains(&buf, expected),
                "expected flag {expected} for atno={atno} needed={needed} worked_before={worked_before}"
            );
        }
    }

    #[tokio::test]
    async fn entity_name_sourced_from_dx_stations() {
        let mut app = fixture_app().await;
        app.decoded_messages.push_back(fixture_view("JA1ABC", 3));
        app.dx_stations.insert(
            "JA1ABC".to_string(),
            crate::app::DxStation {
                call_sign: "JA1ABC".to_string(),
                grid_square: None,
                frequency: 14.074,
                mode: "FT8".to_string(),
                last_seen: chrono::Utc::now(),
                snr: 3,
                distance: None,
                bearing: None,
                worked_before: false,
                needed: false,
                atno: false,
                band_needed: false,
                priority_score: 0,
                local_rank: None,
                network_rank: None,
                source: crate::app::SpotSource::Local,
                entity_name: Some("Japan".to_string()),
                rarity_tier: None,
                reporter_count: None,
                is_notable: false,
                notable_type: None,
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
                audio_offset_hz: Some(1200),
                slot_parity: None,
                watchlisted: false,
            },
        );

        let buf = render(&app);
        assert!(buffer_contains(&buf, "Japan"), "missing entity name");
    }

    #[tokio::test]
    async fn grid_distance_message_snr_render_on_line2() {
        let mut app = fixture_app().await;
        app.decoded_messages.push_back(fixture_view("N2ABC", -7));

        let buf = render(&app);
        assert!(buffer_contains(&buf, "FN42"), "missing grid");
        // fixture_view uses distance=1234.0, which `format_distance`
        // renders as "1.2k km" (>=1000km uses the k-scaled form).
        assert!(buffer_contains(&buf, "1.2k"), "missing distance");
        assert!(buffer_contains(&buf, "CQ N2ABC FN42"), "missing message");
        assert!(buffer_contains(&buf, "-7"), "missing snr");
    }

    #[tokio::test]
    async fn engaged_station_shows_qso_state() {
        let mut app = fixture_app().await;
        app.decoded_messages.push_back(fixture_view("K5ARH", 1));
        app.active_qsos.push(fixture_banner("K5ARH", "wait rpt"));

        let buf = render(&app);
        assert!(
            buffer_contains(&buf, "in QSO: wait rpt"),
            "missing engaged line"
        );
    }

    #[tokio::test]
    async fn unengaged_station_shows_window_and_call_hint() {
        let mut app = fixture_app().await;
        // fixture_view defaults to SlotParity::Even.
        app.decoded_messages.push_back(fixture_view("VK9DX", 0));

        let buf = render(&app);
        assert!(buffer_contains(&buf, "E-window"), "missing window hint");
        assert!(buffer_contains(&buf, "Space=call"), "missing call hint");
    }
}
