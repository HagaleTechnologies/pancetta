//! Station card: a compact panel summarizing the currently-focused station
//! (`App::focused_callsign`) — callsign, DXCC entity + need status, grid,
//! distance, last-heard message, SNR, age, and whether we're engaged with
//! them (active QSO) or free to call.
//!
//! This is Task 17 of the pancetta-tui redesign: the renderer in isolation.
//! It is NOT yet wired into any layout function — Task 18 replaces Station
//! Info's slot with it (`layout_operate`) and adds it below DX Hunter in
//! `layout_hunt` when there's room.
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

/// Render the station card into `area`. Three content lines when a station
/// is focused (call/entity/status, grid/distance/last-msg, engaged-or-window),
/// one fallback line when nothing is focused.
pub fn render_station_card(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    // `is_active` styling is hardcoded false — there's no `ActivePanel`
    // variant for this card yet (added when Task 18 wires it into a
    // layout). Not a functional gap for this task: the renderer is tested
    // directly, not via `draw()`.
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

    // Newest decode from the focused station drives lines 1-3 (see module
    // doc for the DxActivityMap divergence).
    let last_decode = newest_decode_for(app, &call);

    let line1 = render_line1(app, &call, last_decode);
    let line2 = render_line2(app, last_decode);
    let line3 = render_line3(app, &call, last_decode);

    let paragraph = Paragraph::new(vec![line1, line2, line3]);
    f.render_widget(paragraph, inner);

    Ok(())
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
    let entity = app
        .dx_stations
        .get(call)
        .and_then(|d| d.entity_name.clone())
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

/// Line 2: `"{grid} {dist}km · {last-msg-text} ({snr:+}) {age}"`.
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

/// Line 3: engaged → `"in QSO: {state}"`; else `"{E|O}-window · Space=call"`
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
mod tests {
    use super::*;
    use crate::app::{ActivePanel, ActiveQsoBanner, App};
    use crate::config::Config;
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture_view(call: &str, snr: i32) -> DecodedMessageView {
        DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14_074_000.0,
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
        }
    }

    async fn fixture_app() -> App {
        App::new(Config::default(), None).await.unwrap()
    }

    fn render(app: &App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(60, 10);
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

    /// The failing test from the brief: a focused, ATNO station renders its
    /// callsign and the ATNO flag; with no focus at all, the fallback
    /// message renders instead.
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
