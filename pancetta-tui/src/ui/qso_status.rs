use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Gauge, Paragraph},
    Frame,
};

use super::{create_panel_block, format_time_ago};
use crate::app::{ActivePanel, App};

/// Resolve a DX station's DXCC entity name for display: prefer cqdx's
/// authoritative `DxStation.entity_name` (already known for anyone who's
/// appeared in DX Hunter as a network/Both spot) and fall back to the
/// offline prefix table for locally-decoded-only stations, matching the
/// pattern `dx_hunter.rs::create_dx_row` and `station_card.rs::render_line1`
/// already use. `None` when neither resolves.
fn resolve_entity(app: &App, call_sign: &str) -> Option<String> {
    app.dx_stations
        .get(call_sign)
        .and_then(|d| d.entity_name.clone())
        .or_else(|| crate::dxcc::entity_for_callsign(call_sign).map(str::to_string))
}

pub fn render_qso_status(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::QsoStatus);
    let active_count = app.qso_statuses.iter().filter(|q| q.active).count();

    let title = if active_count > 1 {
        format!("QSO Status ({}/{})", active_count, app.qso_statuses.len())
    } else {
        "QSO Status".to_string()
    };
    let block = create_panel_block(&title, is_active, app);

    if active_count > 1 {
        // Multi-QSO table view
        let inner = block.inner(area);
        f.render_widget(block, area);
        render_multi_qso_table(f, inner, app);
    } else {
        // Single QSO detail view (original layout). When there are queued
        // cross-parity calls (#40), allocate an extra row for them above
        // the control hint so the operator knows those calls are waiting.
        let queued_height = if app.pending_calls.is_empty() { 0 } else { 1 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // QSO info
                Constraint::Length(3),             // Sequence ladder + Now/Next
                Constraint::Length(2),             // TX/RX status
                Constraint::Length(2),             // SNR meters
                Constraint::Min(1),                // Progress/timing
                Constraint::Length(1),             // Last-10-QSOs history (#165)
                Constraint::Length(queued_height), // Queued calls (0 when empty)
                Constraint::Length(1),             // Control hint
            ])
            .split(block.inner(area));

        f.render_widget(block, area);

        render_qso_info(f, chunks[0], app);
        render_ladder(f, chunks[1], app);
        render_tx_rx_status(f, chunks[2], app);
        render_snr_meters(f, chunks[3], app);
        render_timing_progress(f, chunks[4], app);
        render_qso_history(f, chunks[5], app);
        if !app.pending_calls.is_empty() {
            render_queued_calls(f, chunks[6], app);
        }
        render_control_hint(f, chunks[7], app);
    }

    Ok(())
}

fn render_multi_qso_table(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();

    // Header
    lines.push(Line::from(vec![Span::styled(
        "  Call       Freq     State         Step    SNR  Exch ",
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    )]));

    // Each active QSO. `cursor` selects which row is highlighted; it indexes
    // into the stored `active_qsos`/`qso_statuses` order (same order the
    // selection cursor uses), so the highlight and the abort/re-send target
    // always agree.
    let cursor = app.qso_cursor.min(app.active_qsos.len().saturating_sub(1));
    for (idx, qso) in app.qso_statuses.iter().enumerate() {
        if !qso.active {
            continue;
        }
        let selected = idx == cursor;
        let marker = if selected { "▶ " } else { "  " };
        let call = qso.call_sign.as_deref().unwrap_or("---");
        let freq = qso
            .frequency
            .map_or("---".to_string(), |f| format!("{:.0}", f));
        let state = qso.state.as_deref().unwrap_or("---");
        // Current rung label from the ladder, if any.
        let step = qso
            .ladder_labels
            .get(qso.ladder_index)
            .map(|s| s.as_str())
            .unwrap_or("-");
        let snr = qso.snr_rx.map_or("---".to_string(), |s| format!("{:+}", s));

        let base_fg = if selected {
            app.theme.accent_color()
        } else {
            app.theme.foreground_color()
        };
        let mut call_style = Style::default().fg(base_fg).add_modifier(Modifier::BOLD);
        if selected {
            call_style = call_style.add_modifier(Modifier::REVERSED);
        }

        let mut row = vec![
            Span::styled(format!("{}{:<10}", marker, call), call_style),
            Span::styled(
                format!("{:>7}  ", freq),
                Style::default().fg(app.theme.warning_color()),
            ),
            Span::styled(
                format!("{:<13}", state),
                Style::default().fg(app.theme.accent_color()),
            ),
            Span::styled(
                format!("{:<7} ", step),
                Style::default().fg(app.theme.warning_color()),
            ),
            Span::styled(
                format!("{:>4}  ", snr),
                Style::default().fg(app.theme.success_color()),
            ),
            Span::styled(
                format!("{:>3}", qso.exchange_count),
                Style::default().fg(app.theme.foreground_color()),
            ),
        ];
        // Batch 2 #1: keep-calling watchdog ("Call 4/10 · stops 3:12") so a
        // keep-calling QSO never reads as an infinite loop in the table.
        if let Some(text) = watchdog_line(qso.call_count, qso.max_calls, qso.watchdog_deadline) {
            row.push(Span::styled(
                format!("  {}", text),
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // Hound-mode badge in the table row.
        if qso.hound {
            row.push(Span::styled(
                " [HOUND]",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(row));
    }

    if lines.len() == 1 {
        lines.push(Line::from(Span::styled(
            " No active QSOs",
            Style::default().fg(app.theme.muted_color()),
        )));
    }

    // Cross-parity queued calls (#40): show a compact "Queued: …" line when
    // the pending queue is non-empty, so the operator knows calls are waiting
    // for the active TX window to clear rather than being silently dropped.
    if !app.pending_calls.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                " Queued: ",
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format_queued_line(&app.pending_calls),
                Style::default().fg(app.theme.warning_color()),
            ),
        ]));
    }

    // Control hint for the multi-QSO view. PAN-21: k aborts from any panel;
    // r (re-send) and Up/Down selection still act only while this panel is
    // focused.
    lines.push(Line::from(Span::styled(
        " [k] abort (any panel)  [r] re-send  Up/Down select  (r/select: this panel only)",
        Style::default().fg(app.theme.muted_color()),
    )));

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_qso_info(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    let status_text = if qso.active { "ACTIVE QSO" } else { "STANDBY" };

    let status_style = if qso.active {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted_color())
    };

    let call_text = qso.call_sign.as_deref().unwrap_or("---");
    let freq_text = qso
        .frequency
        .map_or("---".to_string(), |f| format!("{:.0} Hz", f));
    let mode_text = qso.mode.as_deref().unwrap_or("---");
    let state_text = qso.state.as_deref().unwrap_or("---");
    let sent_text = qso
        .report_sent
        .map_or("---".to_string(), |r| format!("{:+}", r));
    let rcvd_text = qso
        .report_received
        .map_or("---".to_string(), |r| format!("{:+}", r));

    // Hound-mode badge: shown only for Hound QSOs, styled distinctively so
    // the operator instantly knows the DXpedition procedure is active.
    let hound_badge: Option<Span<'_>> = if qso.hound {
        Some(Span::styled(
            " [HOUND]",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        None
    };

    let mut status_line = vec![
        Span::styled(
            "Status: ",
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::styled(status_text, status_style),
    ];
    if let Some(badge) = hound_badge {
        status_line.push(badge);
    }
    status_line.push(Span::raw("  "));
    status_line.push(Span::styled(
        "Call: ",
        Style::default().fg(app.theme.foreground_color()),
    ));
    status_line.push(Span::styled(
        call_text,
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));
    // #171: DXCC entity of the station we're in QSO with, when resolvable.
    // Omitted entirely (not "(---)") when unresolved — this panel has less
    // room than DX Hunter's dedicated column.
    if let Some(call) = qso.call_sign.as_deref() {
        if let Some(entity) = resolve_entity(app, call) {
            status_line.push(Span::styled(
                format!(" ({entity})"),
                Style::default().fg(app.theme.muted_color()),
            ));
        }
    }

    let lines = vec![
        Line::from(status_line),
        Line::from(vec![
            Span::styled("Freq: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(freq_text, Style::default().fg(app.theme.warning_color())),
            Span::raw("  "),
            Span::styled("Mode: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(mode_text, Style::default().fg(app.theme.accent_color())),
            Span::raw("  "),
            Span::styled(
                "Exchanges: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                format!("{}", qso.exchange_count),
                Style::default().fg(app.theme.success_color()),
            ),
        ]),
        // Batch 94: state-machine phase + reports exchanged. The QSO
        // engine pushes these live via ActiveQsosUpdate snapshots.
        Line::from(vec![
            Span::styled("State: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                state_text,
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Sent: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(sent_text, Style::default().fg(app.theme.warning_color())),
            Span::raw("  "),
            Span::styled("Rcvd: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(rcvd_text, Style::default().fg(app.theme.success_color())),
        ]),
    ];

    // Batch 2 #1: when manually keep-calling, surface the watchdog so
    // keep-calling never reads as an infinite loop ("Call 4/10 · stops 3:12").
    let mut lines = lines;
    if let Some(text) = watchdog_line(qso.call_count, qso.max_calls, qso.watchdog_deadline) {
        lines.push(Line::from(vec![Span::styled(
            text,
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD),
        )]));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// Format the manual keep-calling watchdog as "Call N/M · stops M:SS".
/// Returns `None` when not keep-calling (`max_calls == 0`). The countdown is
/// recomputed against the live clock each frame from `deadline`; once past the
/// deadline it shows "stops now".
fn watchdog_line(
    call_count: u32,
    max_calls: u32,
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<String> {
    if max_calls == 0 {
        return None;
    }
    let mut s = format!("Call {}/{}", call_count, max_calls);
    if let Some(deadline) = deadline {
        let remaining = deadline.signed_duration_since(chrono::Utc::now());
        let secs = remaining.num_seconds();
        if secs > 0 {
            s.push_str(&format!(" · stops {}:{:02}", secs / 60, secs % 60));
        } else {
            s.push_str(" · stops now");
        }
    }
    Some(s)
}

/// Pick the live-TX item to surface on the QSO Status panel's "Now:" line.
///
/// Returns the keyed item when it belongs to this QSO (matched by `qso_id`)
/// OR when it has no `qso_id` (a manual free-text send — the operator is
/// watching this single-detail panel for the only active QSO). Returns `None`
/// when nothing is keyed, or when the keyed frame belongs to a *different*
/// QSO. Pure so it can be unit-tested without a render backend.
fn live_tx_for_qso<'a>(
    now_sending: Option<&'a crate::app::TxQueueItem>,
    qso_id: Option<&str>,
) -> Option<&'a crate::app::TxQueueItem> {
    now_sending.filter(|item| item.qso_id.is_none() || item.qso_id.as_deref() == qso_id)
}

/// Render the role-aware sequence ladder plus the Now/Next lines for the
/// selected (single-detail) QSO. The ladder shows the canonical exchange
/// as a row of rungs joined by " ── "; rungs we transmit are prefixed with
/// a small marker, the current rung is wrapped in [ ] and highlighted.
fn render_ladder(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // No ladder available (terminal/idle/contest, or no QSO yet): render
    // a quiet placeholder so the layout stays stable.
    if !qso.active || qso.ladder_labels.is_empty() {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "Seq : ---",
            Style::default().fg(app.theme.muted_color()),
        )));
        f.render_widget(paragraph, area);
        return;
    }

    let mut spans: Vec<Span> = vec![Span::styled(
        "Seq : ",
        Style::default().fg(app.theme.foreground_color()),
    )];

    for (i, label) in qso.ladder_labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " ── ",
                Style::default().fg(app.theme.muted_color()),
            ));
        }
        let ours = qso.ladder_ours.get(i).copied().unwrap_or(false);
        let prefix = if ours { "·" } else { "" };
        let is_current = i == qso.ladder_index;
        let text = if is_current {
            format!("[{}{}]", prefix, label)
        } else {
            format!("{}{}", prefix, label)
        };
        let style = if is_current {
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD)
        } else if ours {
            Style::default().fg(app.theme.accent_color())
        } else {
            Style::default().fg(app.theme.foreground_color())
        };
        spans.push(Span::styled(text, style));
    }

    let next = if qso.next_line.is_empty() {
        "---".to_string()
    } else {
        qso.next_line.clone()
    };

    // When a frame is actively keyed for THIS QSO, the "Now:" line shows what
    // is on the air RIGHT NOW (bold red), so the operator watching this panel
    // sees the live transmission, not just the planned next step. Falls back
    // to the planned now_line when nothing is transmitting (or the live frame
    // belongs to a different QSO / a manual send). The match is by qso_id; a
    // live send with no qso_id (manual free-text) is shown too, since the
    // operator is watching the single-detail panel for the only active QSO.
    let live_tx = live_tx_for_qso(app.tx_now_sending.as_ref(), qso.qso_id.as_deref());

    let now_line = match live_tx {
        Some(item) => Line::from(vec![
            Span::styled("Now : ", Style::default().fg(app.theme.muted_color())),
            Span::styled(
                " 🔴 TX ",
                Style::default()
                    .fg(ratatui::style::Color::White)
                    .bg(ratatui::style::Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} @{:.0}Hz", item.text, item.freq_hz),
                Style::default()
                    .fg(ratatui::style::Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None => {
            let now = if qso.now_line.is_empty() {
                "---".to_string()
            } else {
                qso.now_line.clone()
            };
            Line::from(vec![
                Span::styled("Now : ", Style::default().fg(app.theme.muted_color())),
                Span::styled(now, Style::default().fg(app.theme.foreground_color())),
                Span::styled("   Next: ", Style::default().fg(app.theme.muted_color())),
                Span::styled(next, Style::default().fg(app.theme.muted_color())),
            ])
        }
    };

    let lines = vec![Line::from(spans), now_line];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// One-line control hint at the bottom of the single-detail QSO panel.
/// PAN-21: k aborts from any panel; r/select still act only while this
/// panel is focused.
fn render_control_hint(f: &mut Frame<'_>, area: Rect, app: &App) {
    let paragraph = Paragraph::new(Line::from(Span::styled(
        "[k] abort (any panel)  [r] re-send  Up/Down select  (r/select: this panel only)",
        Style::default().fg(app.theme.muted_color()),
    )));
    f.render_widget(paragraph, area);
}

/// Render the cross-parity queued-call row (#40) in the single-detail view.
/// Only called when `app.pending_calls` is non-empty.
fn render_queued_calls(f: &mut Frame<'_>, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(
            "Queued: ",
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format_queued_line(&app.pending_calls),
            Style::default().fg(app.theme.warning_color()),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Render the last-10-QSOs history line (#165).
fn render_qso_history(f: &mut Frame<'_>, area: Rect, app: &App) {
    let items: Vec<_> = app.qso_history.iter().cloned().collect();
    let spans = format_qso_history_line(&items);
    let line = if spans.is_empty() {
        Line::from(Span::styled(
            "No QSOs yet",
            Style::default().fg(app.theme.muted_color()),
        ))
    } else {
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Build the compact text for the "Queued:" line from the pending-call list.
/// Each entry is rendered as "CALLSIGN (waiting Even, 1:30)" separated by
/// "  •  ". Pure function so it is directly unit-testable.
pub(crate) fn format_queued_line(pending: &[crate::app::PendingCallBanner]) -> String {
    use pancetta_core::slot::SlotParity;
    pending
        .iter()
        .map(|p| {
            // The DX's parity is the window we CANNOT use (we'd TX on the
            // opposite). Describe what we're waiting for clearly.
            let waiting_for = match p.dx_parity {
                Some(SlotParity::Even) => "Odd window",
                Some(SlotParity::Odd) => "Even window",
                None => "a window",
            };
            let elapsed = format_elapsed(p.waited_secs);
            format!("{} (waiting {}, {})", p.callsign, waiting_for, elapsed)
        })
        .collect::<Vec<_>>()
        .join("  •  ")
}

/// Format elapsed seconds as "M:SS" (e.g. "0:45", "3:02").
fn format_elapsed(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Build the last-10-QSOs history line (#165): "✓K5ARH ✗JA1ABC ✓DL5XYZ ...",
/// most recent first (the slice is already ordered that way by
/// `App::push_qso_history`). Not color-only — the ✓/✗ glyph itself carries
/// the meaning. Pure so it's directly unit-testable, following the same
/// pattern as `format_queued_line`.
pub(crate) fn format_qso_history_line(items: &[crate::app::QsoHistoryItem]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let (glyph, color) = if item.success {
            ("\u{2713}", Color::Green)
        } else {
            ("\u{2717}", Color::Red)
        };
        spans.push(Span::styled(
            format!("{glyph}{}", item.call_sign),
            Style::default().fg(color),
        ));
    }
    spans
}

fn render_tx_rx_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // Batch 94: show the last message exchanged in each direction, with
    // a time-ago suffix when the timestamp is known.
    let tx_status = format_direction_line("TX", qso.last_tx_text.as_deref(), qso.last_tx);
    let rx_status = format_direction_line("RX", qso.last_rx_text.as_deref(), qso.last_rx);

    let mut lines = vec![
        Line::from(Span::styled(
            tx_status,
            Style::default().fg(app.theme.warning_color()),
        )),
        Line::from(Span::styled(
            rx_status,
            Style::default().fg(app.theme.success_color()),
        )),
    ];

    // #41: what the DX is doing on the band right now (their latest decoded
    // frame, even before they answer us) — so the operator can tell whether
    // they're working someone else, calling CQ, or coming back to us.
    if let Some(activity) = qso.dx_last_activity.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("DX: {}", activity),
            Style::default().fg(app.theme.muted_color()),
        )));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

/// Build a "TX: JA1ABC K5ARH EM10 (12s ago)" style line for one
/// direction of the last-message display. Falls back gracefully when
/// only the timestamp or neither is known.
fn format_direction_line(
    label: &str,
    text: Option<&str>,
    at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    match (text, at) {
        (Some(text), Some(at)) => format!("{}: {} ({})", label, text, format_time_ago(at)),
        (Some(text), None) => format!("{}: {}", label, text),
        (None, Some(at)) => format!("{}: {}", label, format_time_ago(at)),
        (None, None) => format!("{}: Never", label),
    }
}

fn render_snr_meters(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    // Split into two columns for TX and RX SNR
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // TX SNR Gauge
    let tx_snr = qso.snr_tx.unwrap_or(-50);
    let tx_ratio = ((tx_snr + 50) as f64 / 80.0).clamp(0.0, 1.0); // Map -50 to +30 dB to 0-1

    let tx_gauge = Gauge::default()
        .block(ratatui::widgets::Block::default().title("TX SNR"))
        .gauge_style(Style::default().fg(get_snr_gauge_color(tx_snr, &app.theme)))
        .ratio(tx_ratio)
        .label(format!("{:+} dB", tx_snr));

    f.render_widget(tx_gauge, chunks[0]);

    // RX SNR Gauge
    let rx_snr = qso.snr_rx.unwrap_or(-50);
    let rx_ratio = ((rx_snr + 50) as f64 / 80.0).clamp(0.0, 1.0);

    let rx_gauge = Gauge::default()
        .block(ratatui::widgets::Block::default().title("RX SNR"))
        .gauge_style(Style::default().fg(get_snr_gauge_color(rx_snr, &app.theme)))
        .ratio(rx_ratio)
        .label(format!("{:+} dB", rx_snr));

    f.render_widget(rx_gauge, chunks[1]);
}

fn render_timing_progress(f: &mut Frame<'_>, area: Rect, app: &App) {
    let qso = app.qso_status();

    if !qso.active {
        // Show general monitoring status
        let lines = vec![
            Line::from(vec![Span::styled(
                "Monitoring for calls...",
                Style::default().fg(app.theme.muted_color()),
            )]),
            Line::from(vec![
                Span::styled(
                    "Audio Level: ",
                    Style::default().fg(app.theme.foreground_color()),
                ),
                Span::styled(
                    format!("{:.1}%", app.audio_level * 100.0),
                    Style::default().fg(get_audio_level_color(app.audio_level, &app.theme)),
                ),
            ]),
        ];

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, area);
        return;
    }

    // Show QSO timing and progress
    let duration = if let Some(started) = qso.started_at {
        let elapsed = chrono::Utc::now().signed_duration_since(started);
        format!(
            "Duration: {}m {}s",
            elapsed.num_minutes(),
            elapsed.num_seconds() % 60
        )
    } else {
        "Duration: Unknown".to_string()
    };

    // FT8 cycle timing (15-second cycles)
    let now = chrono::Utc::now();
    let seconds_in_cycle = now.timestamp() % 15;
    let cycle_progress = seconds_in_cycle as f64 / 15.0;

    let cycle_status = if seconds_in_cycle < 13 {
        "RX Period"
    } else {
        "TX Window"
    };

    let lines = vec![Line::from(vec![
        Span::styled(duration, Style::default().fg(app.theme.foreground_color())),
        Span::raw("  "),
        Span::styled(cycle_status, Style::default().fg(app.theme.accent_color())),
    ])];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);

    // Render cycle progress bar if area is tall enough
    if area.height > 2 {
        let progress_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };

        let cycle_gauge = Gauge::default()
            .ratio(cycle_progress)
            .gauge_style(Style::default().fg(app.theme.accent_color()))
            .label(format!("{}s", 15 - seconds_in_cycle));

        f.render_widget(cycle_gauge, progress_area);
    }
}

fn get_snr_gauge_color(snr: i32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match snr {
        snr if snr >= 10 => theme.success_color(),
        snr if snr >= 0 => theme.warning_color(),
        snr if snr >= -10 => theme.warning_color(),
        _ => theme.error_color(),
    }
}

fn get_audio_level_color(level: f32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match level {
        level if level > 0.8 => theme.error_color(), // Too loud
        level if level > 0.1 => theme.success_color(), // Good level
        level if level > 0.01 => theme.warning_color(), // Low but present
        _ => theme.muted_color(),                    // Very low/no signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snr_gauge_color() {
        use crate::config::Theme;
        let theme = Theme::Dark;

        assert_eq!(get_snr_gauge_color(15, &theme), theme.success_color());
        assert_eq!(get_snr_gauge_color(5, &theme), theme.warning_color());
        assert_eq!(get_snr_gauge_color(-15, &theme), theme.error_color());
    }

    /// Batch 2 #1: the watchdog line reads "Call N/M · stops M:SS" while
    /// keep-calling and is absent (None) otherwise.
    #[test]
    fn test_watchdog_line() {
        // Not keep-calling (max_calls == 0) → None.
        assert!(watchdog_line(0, 0, None).is_none());

        // Keep-calling, future deadline → "Call 4/10 · stops M:SS".
        let deadline = chrono::Utc::now() + chrono::Duration::seconds(192);
        let line = watchdog_line(4, 10, Some(deadline)).expect("line");
        assert!(line.starts_with("Call 4/10 · stops "), "got {line}");
        // ~3:12 (allow drift): minutes component present, MM:SS format.
        assert!(line.contains(':'), "got {line}");

        // Past deadline → "stops now".
        let past = chrono::Utc::now() - chrono::Duration::seconds(5);
        let line = watchdog_line(10, 10, Some(past)).expect("line");
        assert_eq!(line, "Call 10/10 · stops now");

        // No deadline → just the count.
        assert_eq!(watchdog_line(2, 10, None).as_deref(), Some("Call 2/10"));
    }

    /// Batch 94: the TX/RX line shows the last message text plus a
    /// time-ago suffix, degrading gracefully when either is missing.
    #[test]
    fn test_format_direction_line() {
        let at = chrono::Utc::now() - chrono::Duration::seconds(12);
        let line = format_direction_line("TX", Some("JA1ABC K5ARH EM10"), Some(at));
        assert!(line.starts_with("TX: JA1ABC K5ARH EM10 ("));
        assert!(line.ends_with(')'));

        assert_eq!(
            format_direction_line("RX", Some("K5ARH JA1ABC -12"), None),
            "RX: K5ARH JA1ABC -12"
        );
        assert_eq!(format_direction_line("RX", None, None), "RX: Never");
        let only_time = format_direction_line("TX", None, Some(at));
        assert!(only_time.starts_with("TX: "));
        assert_ne!(only_time, "TX: Never");
    }

    /// The QSO Status "Now:" line surfaces the live TX frame when it belongs
    /// to the selected QSO (matched by qso_id) or has no qso_id (manual send),
    /// and stays quiet when the keyed frame is for a different QSO.
    #[test]
    fn test_live_tx_for_qso_selection() {
        use super::live_tx_for_qso;
        use crate::app::TxQueueItem;

        let item_a = TxQueueItem {
            text: "W5XO K5ARH R-02".to_string(),
            freq_hz: 1234.0,
            qso_id: Some("qso-a".to_string()),
            deferred: false,
        };
        let item_manual = TxQueueItem {
            text: "CQ K5ARH EM10".to_string(),
            freq_hz: 1500.0,
            qso_id: None,
            deferred: false,
        };

        // Nothing keyed → None.
        assert!(live_tx_for_qso(None, Some("qso-a")).is_none());

        // Keyed frame belongs to this QSO → surfaced.
        assert!(live_tx_for_qso(Some(&item_a), Some("qso-a")).is_some());

        // Keyed frame belongs to a DIFFERENT QSO → not surfaced here.
        assert!(live_tx_for_qso(Some(&item_a), Some("qso-b")).is_none());

        // Manual send (no qso_id) → surfaced for the single-detail panel.
        assert!(live_tx_for_qso(Some(&item_manual), Some("qso-a")).is_some());
        assert!(live_tx_for_qso(Some(&item_manual), None).is_some());
    }

    /// #40: format_queued_line renders each pending call as
    /// "CALL (waiting <parity> window, M:SS)" separated by "  •  ".
    #[test]
    fn test_format_queued_line_empty() {
        assert_eq!(format_queued_line(&[]), "");
    }

    #[test]
    fn test_format_queued_line_single() {
        use crate::app::PendingCallBanner;
        use pancetta_core::slot::SlotParity;
        let pending = vec![PendingCallBanner {
            callsign: "D2UY".to_string(),
            dx_parity: Some(SlotParity::Even), // DX is Even → we want Odd
            waited_secs: 45,
        }];
        let line = format_queued_line(&pending);
        // DX is Even → we're waiting for the Odd window to free up
        assert!(line.contains("D2UY"), "callsign: {line}");
        assert!(line.contains("Odd window"), "parity label: {line}");
        assert!(line.contains("0:45"), "elapsed: {line}");
    }

    #[test]
    fn test_format_queued_line_multiple() {
        use crate::app::PendingCallBanner;
        use pancetta_core::slot::SlotParity;
        let pending = vec![
            PendingCallBanner {
                callsign: "D2UY".to_string(),
                dx_parity: Some(SlotParity::Even),
                waited_secs: 45,
            },
            PendingCallBanner {
                callsign: "VK9XX".to_string(),
                dx_parity: Some(SlotParity::Odd), // DX is Odd → we want Even
                waited_secs: 90,
            },
        ];
        let line = format_queued_line(&pending);
        assert!(line.contains("D2UY"), "first call: {line}");
        assert!(line.contains("VK9XX"), "second call: {line}");
        assert!(line.contains("Even window"), "second parity: {line}");
        assert!(line.contains("1:30"), "second elapsed: {line}");
        assert!(line.contains("  •  "), "separator: {line}");
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(45), "0:45");
        assert_eq!(format_elapsed(90), "1:30");
        assert_eq!(format_elapsed(3 * 60 + 5), "3:05");
    }

    #[test]
    fn format_qso_history_line_empty_is_empty() {
        assert!(format_qso_history_line(&[]).is_empty());
    }

    #[test]
    fn format_qso_history_line_renders_glyph_per_outcome() {
        use crate::app::QsoHistoryItem;
        let items = vec![
            QsoHistoryItem {
                call_sign: "K5ARH".to_string(),
                success: true,
                completed_at: chrono::Utc::now(),
            },
            QsoHistoryItem {
                call_sign: "JA1ABC".to_string(),
                success: false,
                completed_at: chrono::Utc::now(),
            },
        ];
        let spans = format_qso_history_line(&items);
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains('\u{2713}'),
            "expected a ✓ glyph: {rendered}"
        ); // ✓
        assert!(
            rendered.contains('\u{2717}'),
            "expected a ✗ glyph: {rendered}"
        ); // ✗
        assert!(rendered.contains("K5ARH"));
        assert!(rendered.contains("JA1ABC"));
    }

    fn test_dx_station(call: &str) -> crate::app::DxStation {
        crate::app::DxStation {
            call_sign: call.to_string(),
            grid_square: None,
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            last_seen: chrono::Utc::now(),
            snr: -10,
            distance: None,
            bearing: None,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: 0,
            source: crate::app::SpotSource::Local,
            entity_name: None,
            rarity_tier: None,
            reporter_count: None,
            is_notable: false,
            notable_type: None,
            confidence: None,
            best_snr_network: None,
            last_seen_network: None,
            audio_offset_hz: None,
            slot_parity: None,
            watchlisted: false,
        }
    }

    #[tokio::test]
    async fn resolve_entity_prefers_cqdx_over_offline_table() {
        use crate::app::DxStation;
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.dx_stations.insert(
            "JA1ABC".to_string(),
            DxStation {
                entity_name: Some("Nippon (cqdx)".to_string()),
                ..test_dx_station("JA1ABC")
            },
        );
        assert_eq!(
            resolve_entity(&app, "JA1ABC").as_deref(),
            Some("Nippon (cqdx)")
        );
    }

    #[tokio::test]
    async fn resolve_entity_falls_back_to_offline_table() {
        use crate::app::DxStation;
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        // In dx_stations (e.g. from a local decode) but cqdx never set entity_name.
        app.dx_stations
            .insert("JA1ABC".to_string(), test_dx_station("JA1ABC"));
        assert_eq!(resolve_entity(&app, "JA1ABC").as_deref(), Some("Japan"));
    }

    #[tokio::test]
    async fn resolve_entity_falls_back_to_offline_table_when_never_seen() {
        let app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        // Not in dx_stations at all (e.g. QSO partner never appeared in DX Hunter).
        assert_eq!(
            resolve_entity(&app, "DL1ABC").as_deref(),
            Some("Fed. Rep. of Germany")
        );
    }

    #[tokio::test]
    async fn resolve_entity_none_when_unresolvable() {
        let app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        assert_eq!(resolve_entity(&app, "QZ9ZZ"), None);
    }
}
