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
use crate::app::{App, DecodedMessageView, RecentQsoOutcome};
use crate::dx_need::EntityStanding;
use crate::upload_status::QsoUploadStatus;

/// PAN-344: upload-detail lines for the SELECTED Recent-QSOs contact's
/// Station card. Driven by the selected QSO's UUID — never the live
/// `focused_callsign` — so an older contact's results are never attributed
/// to a live station with the same callsign. Long reasons clip with `…`,
/// retaining target and state.
pub fn upload_detail_lines(
    qso: &RecentQsoOutcome,
    status: Option<&QsoUploadStatus>,
    width: u16,
) -> Vec<Line<'static>> {
    let muted = |text: String| {
        Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    let limit = width.saturating_sub(1) as usize;
    let clip = |text: String| -> String {
        if text.chars().count() > limit {
            let prefix: String = text.chars().take(limit.saturating_sub(1)).collect();
            format!("{prefix}\u{2026}")
        } else {
            text
        }
    };
    match (&qso.outcome, status) {
        (crate::app::QsoOutcome::Failed(_), _) => vec![muted("Uploads: not applicable".into())],
        (_, None) => vec![muted("Uploads: checking\u{2026}".into())],
        (_, Some(status)) if status.targets.is_empty() => {
            vec![muted("Uploads: disabled".into())]
        }
        (_, Some(status)) => status
            .targets
            .iter()
            .map(|(target, slot)| {
                let detail = if slot.reason.is_empty() || slot.reason == slot.state.word() {
                    slot.state.word().to_string()
                } else {
                    format!("{} \u{b7} {}", slot.state.word(), slot.reason)
                };
                Line::from(Span::styled(
                    clip(format!("{}: {}", target.label(), detail)),
                    Style::default(),
                ))
            })
            .collect(),
    }
}

/// Render four content lines: call/entity/status, ranking reason,
/// grid/distance/last-message, and engagement or transmit window;
/// an optional QRZ identity line after the callsign, or one fallback line
/// when nothing is focused.
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
    let mut lines = vec![line1];
    if let Some(info) = app.qrz_stations.get(&call.trim().to_uppercase()) {
        let parts: Vec<&str> = [&info.name, &info.qth, &info.grid, &info.country]
            .into_iter()
            .filter_map(|s| s.as_deref())
            .filter(|s| !s.is_empty())
            .collect();
        lines.push(Line::from(format!("QRZ: {}", parts.join(" · "))));
    }
    lines.extend([why, line2, line3]);
    let paragraph = Paragraph::new(lines);
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

/// Line 1: `"{call} — {entity|'---'} {ATNO★|needed|confirmed|worked-unconfirmed|worked|new}"`.
/// `confirmed` / `worked-unconfirmed` come from the local logbook (PAN-347).
fn render_line1<'a>(app: &App, call: &str, last_decode: Option<&DecodedMessageView>) -> Line<'a> {
    // PAN-85: append the station's US state/territory when known, matching
    // DX Hunter. Deliberately no offline `pancetta_core::dxcc` fallback
    // (`DxStation::offline_entity`) here — out of scope (see D10).
    let entity = app
        .dx_stations
        .get(call)
        .and_then(|d| d.entity_name.clone())
        .map(|e| pancetta_core::dxcc::format_entity_with_state(&e, app.station_state_for(call)))
        .unwrap_or_else(|| "---".to_string());

    // Worked status is per exact call: a newer decode of the base or
    // portable form must not hide this call's own status (PAN-228).
    let status_decode = app
        .decoded_messages
        .iter()
        .rev()
        .find(|m| {
            m.call_sign
                .as_deref()
                .is_some_and(|c| crate::app::same_station(c, call))
        })
        .or(last_decode);
    // PAN-93: read this frame's cqdx verdict for the row the Why line explains.
    let row_key = app
        .dx_station_for(call)
        .map_or(call, |row| row.call_sign.as_str());
    let standing = app.dx_need_for(row_key).and_then(|n| n.entity_standing);
    let status_flag = status_decode
        .map(|m| {
            let (needed, atno) = app.dx_need_flags_shown(row_key, m.needed, m.atno);
            if atno {
                "ATNO\u{2605}"
            } else if needed {
                "needed"
            } else if standing == Some(EntityStanding::Confirmed) {
                "confirmed"
            } else if standing == Some(EntityStanding::WorkedUnconfirmed) {
                "worked-unconfirmed"
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

    #[test]
    fn pan344_selected_qso_upload_details() {
        use crate::app::{QsoOutcome, RecentQsoOutcome};
        use crate::upload_status::{
            QsoUploadStatus, TargetUploadStatus, UploadState, UploadTarget,
        };
        let qso = |outcome: QsoOutcome| RecentQsoOutcome {
            callsign: "K1ABC".to_string(),
            qso_uuid: "u-1".to_string(),
            outcome,
            last_state: "Completed".to_string(),
            freq_hz: 14_075_000,
            ts: chrono::Utc::now(),
            brief_timeline: vec!["Completed".to_string()],
        };
        let row = |lines: &[Line<'static>]| -> String {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };

        // Mixed states: each target's state and reason, in order.
        let status = QsoUploadStatus {
            uuid: "u-1".into(),
            callsign: "K1ABC".into(),
            revision: 3,
            targets: vec![
                (
                    UploadTarget::Qrz,
                    TargetUploadStatus::new(UploadState::Accepted, ""),
                ),
                (
                    UploadTarget::ClubLog,
                    TargetUploadStatus::pending("retrying \u{b7} temporary service failure"),
                ),
                (
                    UploadTarget::Eqsl,
                    TargetUploadStatus::new(
                        UploadState::Rejected,
                        "record rejected \u{b7} invalid record",
                    ),
                ),
            ],
        };
        let text = row(&upload_detail_lines(
            &qso(QsoOutcome::Completed),
            Some(&status),
            70,
        ));
        assert!(text.contains("QRZ Logbook: accepted"), "{text}");
        assert!(
            text.contains("ClubLog: pending \u{b7} retrying \u{b7} temporary service failure"),
            "{text}"
        );
        assert!(
            text.contains("eQSL: rejected \u{b7} record rejected \u{b7} invalid record"),
            "{text}"
        );

        // Completion before any snapshot: checking.
        assert!(
            row(&upload_detail_lines(&qso(QsoOutcome::Completed), None, 70))
                .contains("Uploads: checking\u{2026}")
        );
        // Confirmed empty target set: disabled.
        let empty = QsoUploadStatus {
            targets: Vec::new(),
            ..status.clone()
        };
        assert!(row(&upload_detail_lines(
            &qso(QsoOutcome::Completed),
            Some(&empty),
            70
        ))
        .contains("Uploads: disabled"));
        // Failed QSO: uploads not applicable.
        assert!(row(&upload_detail_lines(
            &qso(QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout)),
            Some(&status),
            70
        ))
        .contains("Uploads: not applicable"));
        // Long reasons clip with …, retaining target and state.
        let long = QsoUploadStatus {
            targets: vec![(
                UploadTarget::Qrz,
                TargetUploadStatus::pending(&"service unavailable because ".repeat(10)),
            )],
            ..status
        };
        let clipped = row(&upload_detail_lines(
            &qso(QsoOutcome::Completed),
            Some(&long),
            40,
        ));
        assert!(clipped.contains("QRZ Logbook: pending"), "{clipped}");
        assert!(clipped.ends_with('\u{2026}'), "{clipped}");
        assert!(clipped.chars().count() <= 40, "{clipped}");
    }

    #[tokio::test]
    async fn qrz_line_is_second_and_only_for_focused_station() {
        let mut app = ranked_app("DL1ABC", true, false).await;
        assert!(!buffer_contains(&render_at(&app, 130, 7), "QRZ:"));
        app.qrz_stations.insert(
            "DL1ABC".into(),
            crate::app::QrzStationInfo {
                name: Some("Hans Müller".into()),
                qth: Some("Amberg".into()),
                grid: Some("JO62jk".into()),
                country: Some("Germany".into()),
                state: None,
            },
        );
        let buf = render_at(&app, 130, 7);
        assert!(row(&buf, 2).contains("QRZ: Hans Müller · Amberg · JO62jk · Germany"));
        assert!(row(&buf, 3).contains("Why:"));
        app.dx_stations.clear();
        app.add_decoded_message(fixture_view("JA1ABC", -7))
            .await
            .unwrap();
        assert!(!buffer_contains(&render_at(&app, 130, 7), "QRZ:"));
        app.qrz_stations.insert(
            "JA1ABC".into(),
            crate::app::QrzStationInfo {
                country: Some("Japan".into()),
                ..Default::default()
            },
        );
        let buf = render_at(&app, 130, 7);
        assert!(buffer_contains(&buf, "QRZ: Japan"));
        assert!(!buffer_contains(&buf, "Amberg"));
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

    /// PAN-228: after a completed QSO, with no later decode, the card and
    /// the Why line agree the station is already worked.
    #[tokio::test]
    async fn completed_qso_card_and_why_line_say_worked() {
        let mut app = ranked_app("JA1ABC", false, false).await;
        assert!(buffer_contains(&render_at(&app, 130, 6), " new"));
        let mut factors = app.dx_stations["JA1ABC"]
            .local_rank
            .unwrap()
            .factors
            .unwrap();
        factors.worked_on_band = true;
        app.refresh_local_rank("JA1ABC", 900, factors);
        let buf = render_at(&app, 130, 6);
        let line1 = row(&buf, 1);
        assert!(line1.contains(" worked"), "{line1}");
        assert!(!line1.contains(" new"), "{line1}");
        let why = row(&buf, 2);
        assert!(why.contains("Why: "), "{why}");
        assert!(why.contains("already worked on 20M"), "{why}");
    }

    /// PAN-228 (Codex #493): a newer decode of the bare call must not hide
    /// the completed portable call's worked status.
    #[tokio::test]
    async fn completed_portable_call_stays_worked_after_a_bare_decode() {
        let mut app = ranked_app("K1ABC/P", false, false).await;
        let mut factors = app.dx_stations["K1ABC/P"]
            .local_rank
            .unwrap()
            .factors
            .unwrap();
        factors.worked_on_band = true;
        app.refresh_local_rank("K1ABC/P", 900, factors);
        app.decoded_messages.push_back(fixture_view("K1ABC", -5));
        assert_eq!(app.focused_callsign().as_deref(), Some("K1ABC/P"));
        let line1 = row(&render_at(&app, 130, 6), 1);
        assert!(line1.contains(" worked"), "{line1}");
        assert!(!line1.contains(" new"), "{line1}");
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
    async fn card_shows_local_standing_below_atno_and_needed() {
        // PAN-347: the local logbook's standing sits below ATNO and needed.
        struct Local(Option<bool>, Option<EntityStanding>);
        impl crate::dx_need::DxNeedLookup for Local {
            fn lookup_need(
                &self,
                _: &str,
                _: Option<&'static str>,
                _: Option<&str>,
                _: Option<&str>,
                _: f64,
                _mode: Option<&str>,
            ) -> crate::dx_need::DxNeedStatus {
                crate::dx_need::DxNeedStatus {
                    dxcc_needed: self.0,
                    entity_standing: self.1,
                    ..Default::default()
                }
            }
        }
        let line1 = |app: &App| row(&render_at(app, 130, 6), 1);
        use EntityStanding::*;
        for (needed, atno, worked_before, verdict, standing, expected) in [
            (
                false,
                false,
                true,
                None,
                Some(WorkedUnconfirmed),
                "worked-unconfirmed",
            ),
            (false, false, false, None, Some(Confirmed), "confirmed"),
            (false, false, false, None, Some(NotWorked), "new"),
            (false, false, true, None, None, "worked"),
            (
                true,
                true,
                false,
                Some(true),
                Some(NotWorked),
                "ATNO\u{2605}",
            ),
            (false, false, false, Some(true), Some(Confirmed), "needed"),
        ] {
            let mut app = fixture_app().await;
            let mut view = fixture_view("W1XYZ", -5);
            view.needed = needed;
            view.atno = atno;
            view.worked_before = worked_before;
            app.add_decoded_message(view).await.unwrap();
            app.refresh_dx_need(Some(&Local(verdict, standing)));
            let line = line1(&app);
            assert!(
                line.contains(&format!(" {expected} ")),
                "expected {expected} for {standing:?}: {line}"
            );
        }
    }

    #[tokio::test]
    async fn needed_flag_follows_this_frames_cqdx_verdict() {
        // PAN-93: when the cqdx list loads or changes after the station's
        // last decode, the card agrees with DX Hunter's `+` and the Why view.
        struct Verdict(Option<bool>);
        impl crate::dx_need::DxNeedLookup for Verdict {
            fn lookup_need(
                &self,
                _: &str,
                _: Option<&'static str>,
                _: Option<&str>,
                _: Option<&str>,
                _: f64,
                _mode: Option<&str>,
            ) -> crate::dx_need::DxNeedStatus {
                crate::dx_need::DxNeedStatus {
                    dxcc_needed: self.0,
                    ..Default::default()
                }
            }
        }
        let line1 = |app: &App| row(&render_at(app, 130, 6), 1);
        // (needed, atno) stored at decode time, then this frame's verdict.
        for (needed, atno, verdict, expected) in [
            (false, false, Some(true), "needed"),
            (true, false, Some(false), "new"),
            (true, true, Some(false), "new"),
            (true, true, Some(true), "ATNO\u{2605}"),
            (true, false, None, "needed"),
        ] {
            let mut app = fixture_app().await;
            let mut view = fixture_view("W1XYZ", -5);
            view.needed = needed;
            view.atno = atno;
            app.add_decoded_message(view).await.unwrap();
            app.refresh_dx_need(Some(&Verdict(verdict)));

            let line1 = line1(&app);
            assert!(
                line1.contains(&format!(" {expected} ")),
                "needed={needed} atno={atno} verdict={verdict:?}: {line1}"
            );
        }

        // The focus names the base call while the DX row is the portable
        // form: the card reads the verdict for the row the Why line uses.
        let mut app = fixture_app().await;
        app.add_decoded_message(fixture_view("W1XYZ/P", -5))
            .await
            .unwrap();
        app.decoded_messages.clear();
        app.decoded_messages.push_back(fixture_view("W1XYZ", -5));
        app.refresh_dx_need(Some(&Verdict(Some(true))));
        assert_eq!(app.focused_callsign().as_deref(), Some("W1XYZ"));
        let line1 = line1(&app);
        assert!(line1.contains(" needed "), "{line1}");
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
                band: None,
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
                offline_entity: pancetta_core::dxcc::entity_for_callsign("JA1ABC"),
                rarity_tier: None,
                reporter_count: None,
                is_notable: false,
                notable_type: None,
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
                audio_offset_hz: Some(1200),
                slot_parity: None,
                spot_rf_hz: None,
                watchlisted: false,
                pinned: false,
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
        assert!(buffer_contains(&buf, "1234 km"), "missing distance");
        app.decoded_messages.back_mut().unwrap().distance = Some(1834.0);
        assert!(buffer_contains(&render(&app), "1834 km"));
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
