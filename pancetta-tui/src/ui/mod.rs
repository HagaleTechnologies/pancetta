use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::{ActivePanel, App};
use crate::widgets::Waterfall;

pub mod active_qsos;
pub mod band_activity;
pub mod callers;
pub mod dx_hunter;
pub mod qso_status;
pub mod station_info;
pub mod tx_placement;

use active_qsos::render_active_qsos;
use band_activity::render_band_activity;
use callers::render_callers;
use dx_hunter::render_dx_hunter;
use qso_status::render_qso_status;
use station_info::render_station_info;
use tx_placement::render_tx_placement;

/// Main UI rendering function
pub fn draw(f: &mut Frame<'_>, app: &App) -> Result<()> {
    let size = f.area();

    // Paint an opaque full-frame background first so every cell carries an
    // explicit bg. This guarantees the alternate screen fully covers the
    // terminal's pre-launch scrollback even where a widget paints nothing
    // (e.g. an empty waterfall when audio is silent) — those gaps would
    // otherwise show through.
    f.render_widget(
        Block::default().style(Style::default().bg(app.theme.background_color())),
        size,
    );

    // Minimum-size guard. Below this the multi-panel layout degrades into
    // unreadable empty boxes (and panels silently drop content); show an
    // explicit resize prompt instead so a new operator isn't staring at a
    // broken-looking screen wondering what's wrong.
    const MIN_W: u16 = 80;
    const MIN_H: u16 = 20;
    if size.width < MIN_W || size.height < MIN_H {
        let msg = vec![
            Line::from(Span::styled(
                "Terminal too small",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "Have {}x{}, need at least {}x{}.",
                size.width, size.height, MIN_W, MIN_H
            )),
            Line::from("Resize the window (or your SSH/terminal) and the UI returns."),
        ];
        let p = Paragraph::new(msg)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true });
        // Vertically center-ish.
        let y = size.height / 2;
        let area = Rect {
            x: size.x,
            y: size.y + y.saturating_sub(1).min(size.height.saturating_sub(1)),
            width: size.width,
            height: size.height.saturating_sub(y).clamp(1, 3),
        };
        f.render_widget(p, area);
        return Ok(());
    }

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Title bar (incl. bold TX-policy banner)
            Constraint::Min(1),    // Main content
            Constraint::Length(1), // TX queue / now-sending strip
            Constraint::Length(3), // Status bar
        ])
        .split(size);

    // Render title bar
    render_title_bar(f, chunks[0], app);

    // Panel zoom (`z`): the focused panel fills the whole content area,
    // bypassing the active view's grid entirely. Orthogonal to the 4-view
    // system — checked before the view dispatch so it applies identically
    // regardless of `app.active_view`.
    if app.zoomed {
        render_zoomed_panel(f, chunks[1], app)?;
    } else {
        // Per-view content layout dispatch. Operate is today's layout
        // (extracted verbatim below, byte-identical); Monitor is the
        // vertical big-picture layout; Hunt is DX-Hunter-first (band
        // activity narrowed to CQs via `App::displayed_messages`); Run is
        // Callers-first for working a pileup.
        match app.active_view {
            crate::view::ActiveView::Operate => layout_operate(f, chunks[1], app)?,
            crate::view::ActiveView::Monitor => layout_monitor(f, chunks[1], app)?,
            crate::view::ActiveView::Hunt => layout_hunt(f, chunks[1], app)?,
            crate::view::ActiveView::Run => layout_run(f, chunks[1], app)?,
        }
    }

    // Render status bar
    // TX queue / now-sending strip (between content and status bar).
    render_tx_strip(f, chunks[2], app);

    render_status_bar(f, chunks[3], app);

    Ok(())
}

/// Renders the focused panel (`app.active_panel`) full-screen in `area`,
/// bypassing whichever `ActiveView` grid is normally in effect. Calls the
/// SAME `render_*` function each view's grid layout uses, just with the
/// whole content rect instead of a sliced-down chunk. Covers all 5
/// `ActivePanel` variants, including `StationInfo` (still valid until
/// Task 18 removes it from the panel cycle).
fn render_zoomed_panel(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    match app.active_panel {
        ActivePanel::BandActivity => render_band_activity(f, area, app)?,
        ActivePanel::QsoStatus => render_qso_status(f, area, app)?,
        ActivePanel::StationInfo => render_station_info(f, area, app)?,
        ActivePanel::Callers => render_callers(f, area, app)?,
        ActivePanel::DxHunter => render_dx_hunter(f, area, app)?,
        ActivePanel::TxPlacement => render_tx_placement(f, area, app)?,
    }
    Ok(())
}

/// Today's Operate-view content layout: a FULL-WIDTH active-QSO banner and
/// TX-placement instrument across the top, then a two-column lower region.
/// The bottom row of each column lines up — QSO Status (left) sits directly
/// across from Callers (right) — with DX Hunter moved up above Callers to
/// make room for the wide strip.
///
/// The energy waterfall (originally `Percentage(30)` here) was swapped for
/// the vacancy-first TX-placement instrument (`Length(7)`, Task 11) — the
/// waterfall remains only in Monitor view (`layout_monitor`). The lower
/// region is `Min(1)` (unconstrained-but-fills-remainder), so it
/// automatically absorbs the space freed by the fixed-height swap; no
/// further constraint changes were needed to "give the freed rows to the
/// largest table" — that table (the two-column grid as a whole) already
/// grows via `Min(1)`, and each column's internal Percentage split still
/// allocates the largest per-column share to the panel it already favored
/// (Band Activity / Callers).
///
/// Extracted verbatim from `draw()` (Task 5) — the two-column lower region
/// still renders byte-identically to the pre-extraction layout; the top row
/// visibly changed in this task (expected — see the Task 11 brief's Global
/// Constraint note).
fn layout_operate(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Active-QSOs banner (full width, 1 row)
            Constraint::Length(7), // TX Placement (full width; 5 content rows + 2 border)
            Constraint::Min(1),    // Lower region (two columns)
        ])
        .split(content_area);

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(content[2]);

    // Left column: Band Activity (top), QSO Status (bottom).
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(60), // Band activity
            Constraint::Percentage(40), // QSO status
        ])
        .split(lower[0]);

    // Right column: Station Info (small) on top, DX Hunter (moved up) in the
    // middle, Callers on the bottom — aligned with QSO Status across the gutter.
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20), // Station Info
            Constraint::Percentage(40), // DX Hunter
            Constraint::Percentage(40), // Callers (bottom; across from QSO Status)
        ])
        .split(lower[1]);

    // Render panels
    render_active_qsos(f, content[0], app);
    render_tx_placement(f, content[1], app)?;
    render_band_activity(f, left_chunks[0], app)?;
    render_qso_status(f, left_chunks[1], app)?;
    render_station_info(f, right_chunks[0], app)?;
    render_dx_hunter(f, right_chunks[1], app)?;
    render_callers(f, right_chunks[2], app)?;

    // Render active panel highlight. The slice order MUST match the
    // ActivePanel enum order used by render_active_panel_highlight:
    // BandActivity, QsoStatus, StationInfo, Callers, DxHunter, TxPlacement.
    // The full-width banner (content[0]) is not a navigable panel.
    render_active_panel_highlight(
        f,
        app,
        &[
            left_chunks[0],  // BandActivity
            left_chunks[1],  // QsoStatus
            right_chunks[0], // StationInfo
            right_chunks[2], // Callers
            right_chunks[1], // DxHunter
            content[1],      // TxPlacement
        ],
    );

    Ok(())
}

/// Monitor-view content layout: a vertical stack — full-width active-QSO
/// banner, a big waterfall, and full-width Band Activity — with no side
/// panels (QSO Status / Station Info / DX Hunter / Callers). Meant for a
/// glance-and-walk-away big-picture view.
///
/// No `render_active_panel_highlight` call here: Band Activity is the only
/// navigable panel in this view, and `render_band_activity`'s own
/// `create_panel_block` already draws an active-styled border when it's the
/// active panel, so the highlight overlay would be redundant (and — see
/// `render_active_panel_highlight` — would blank the panel's title, since it
/// draws a titleless border directly on top).
fn layout_monitor(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Percentage(60), // Big waterfall (full width)
            Constraint::Min(1),         // Band Activity (full width)
        ])
        .split(content_area);

    render_active_qsos(f, content[0], app);
    render_waterfall(f, content[1], app);
    render_band_activity(f, content[2], app)?;

    Ok(())
}

/// Hunt-view content layout: DX Hunter gets top billing (full width) for
/// picking a rare/needed station to chase, the TX-placement instrument sits
/// underneath (per-stream markers matter here too — Task 11), Band Activity
/// shows the narrowed CQs-only feed, and QSO Status anchors the bottom so
/// the operator can track an in-progress call. No Callers, no Station Info,
/// no energy waterfall (that stays in Monitor only) — this view is about
/// hunting, not answering.
///
/// Task 6 never gave Hunt a waterfall row to "replace", so the placement
/// strip is new real estate here rather than a swap; DX Hunter (the
/// dominant table per the design spec) keeps its 45% share, and Band
/// Activity's share shrinks (35%→25%) to make room instead.
///
/// No `render_active_panel_highlight` call, same rationale as
/// `layout_monitor`: each panel renderer draws its own active-styled border
/// via `create_panel_block`.
fn layout_hunt(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Length(7),      // TX Placement (full width)
            Constraint::Percentage(45), // DX Hunter (full width)
            Constraint::Percentage(25), // Band Activity (full width, CQs-only)
            Constraint::Min(5),         // QSO Status
        ])
        .split(content_area);

    render_active_qsos(f, content[0], app);
    render_tx_placement(f, content[1], app)?;
    render_dx_hunter(f, content[2], app)?;
    render_band_activity(f, content[3], app)?;
    render_qso_status(f, content[4], app)?;

    Ok(())
}

/// Run-view content layout: Callers gets top billing (full width) for
/// working stations calling us, the TX-placement instrument sits underneath
/// (its per-stream TX markers matter most here — serving a pileup), and QSO
/// Status anchors the bottom in its existing multi-table ("Active QSOs")
/// mode so the operator can track several concurrent exchanges at once. No
/// DX Hunter, no Band Activity, no Station Info, no energy waterfall — this
/// view is about answering, not hunting.
///
/// Task 6 never gave Run a waterfall row to "replace" either (see
/// `layout_hunt`'s doc comment); Callers (the dominant table) keeps its 50%
/// share, and QSO Status's `Min(5)` absorbs the new fixed-height row same as
/// it already absorbed the pre-existing remainder.
///
/// No `render_active_panel_highlight` call, same rationale as
/// `layout_monitor`: each panel renderer draws its own active-styled border
/// via `create_panel_block`.
fn layout_run(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Length(7),      // TX Placement (full width)
            Constraint::Percentage(50), // Callers (full width)
            Constraint::Min(5),         // QSO Status (multi-table "Active QSOs" mode)
        ])
        .split(content_area);

    render_active_qsos(f, content[0], app);
    render_tx_placement(f, content[1], app)?;
    render_callers(f, content[2], app)?;
    render_qso_status(f, content[3], app)?;

    Ok(())
}

fn render_title_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let utc_clock = chrono::Utc::now().format("%H:%M:%S UTC").to_string();

    let mut left_spans = vec![
        Span::styled(
            "Pancetta TUI",
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.call_sign,
            Style::default()
                .fg(app.theme.success_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.grid_square,
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("{:.3} MHz", app.station_info.operating_frequency),
            Style::default().fg(app.theme.warning_color()),
        ),
        Span::raw(" "),
        Span::styled(
            app.config
                .get_current_band(app.station_info.operating_frequency)
                .map(|b| b.name.as_str())
                .unwrap_or(""),
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.mode,
            Style::default().fg(app.theme.accent_color()),
        ),
    ];

    // Bold, color-coded global TX-policy banner chip. Always visible so
    // the operator can tell at a glance which of the three states is
    // active: GREEN "TX: FULL", YELLOW "TX: RESPOND-ONLY", RED
    // "TX: DISABLED — RX ONLY". Reversed/bold for visual dominance.
    let (policy_text, policy_bg) = match app.tx_policy {
        pancetta_core::TxPolicy::Full => (" TX: FULL ".to_string(), Color::Green),
        pancetta_core::TxPolicy::RespondOnly => (" TX: RESPOND-ONLY ".to_string(), Color::Yellow),
        pancetta_core::TxPolicy::Disabled => (" TX: DISABLED — RX ONLY ".to_string(), Color::Red),
    };
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        policy_text,
        Style::default()
            .fg(Color::Black)
            .bg(policy_bg)
            .add_modifier(Modifier::BOLD),
    ));

    // TX-frequency mode chip: HOLD (operator's offset is sticky) vs AUTO
    // (pancetta picks/adjusts). Cyan = HOLD (locked), Magenta = AUTO (free).
    let (freq_text, freq_bg) = match app.tx_freq_mode {
        pancetta_core::TxFreqMode::Hold => (" FREQ: HOLD ".to_string(), Color::Cyan),
        pancetta_core::TxFreqMode::Auto => (" FREQ: AUTO ".to_string(), Color::Magenta),
    };
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        freq_text,
        Style::default()
            .fg(Color::Black)
            .bg(freq_bg)
            .add_modifier(Modifier::BOLD),
    ));

    // Active operating-mode chip: shown only when the station mode is NOT FT8
    // (e.g. cyan "FT4"). FT8 ⇒ no chip ⇒ title bar byte-identical to today.
    if let Some(label) = mode_chip_label(&app.station_info.mode) {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Active-view chip (Phase 2 TUI redesign): shown only when the operator
    // has switched away from Operate (the default) — `label()` returns
    // `None` for Operate, so the title bar is byte-identical to today until
    // the operator presses `v`/`V`.
    if let Some(label) = app.active_view.label() {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Split-TX chip: shown when the rig is operating split (TX ≠ RX dial).
    if app.split_tx_hz != 0 {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" SPLIT TX {:.3} ", app.split_tx_hz as f64 / 1e6),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Fox-mode chip: shown when Fox (DXpedition operator) mode is engaged.
    // Magenta/bold so it stands out from the cyan SPLIT chip and the green TX
    // chip. Off ⇒ no chip, no change to the title bar.
    if app.fox_mode {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            " FOX ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // TX audio offset chip: shown when the operator has set a held offset.
    // "TX off: NNNN (HOLD)" when set; hidden when Auto (no noise in the bar).
    if let Some(offset_hz) = app.tx_offset_hold_hz {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" TX off: {} (HOLD) ", offset_hz),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Health alarm chip — prominent, always-visible warning for the highest-
    // stakes silent failures so a new operator (or one whose audio device was
    // hijacked by a remote-desktop client) sees *why* nothing is decoding,
    // rather than just an empty waterfall. Driven by the existing pipeline
    // health snapshot; the bottom status bar still shows the per-stage detail.
    if let Some(ref h) = app.pipeline_health {
        let alarm = if !h.audio_alive {
            Some(" ⚠ AUDIO DEAD — press d ")
        } else if !h.ft8lib_available {
            Some(" ⚠ DECODER STUB ")
        } else {
            None
        };
        if let Some(text) = alarm {
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                text,
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // FCC §97.221 presence prompt: autonomous on, but operator idle → initiation
    // (CQ/pounce) is suppressed until they prove presence with a keypress.
    if app.autonomous_init_paused {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            " ⏸ AUTO-CQ PAUSED — press a key (FCC §97.221) ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // TX indicator
    if app.is_transmitting {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            " TX ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Calculate padding to right-align the UTC clock
    let left_len: usize = left_spans.iter().map(|s| s.width()).sum();
    let clock_len = utc_clock.len();
    let padding = (area.width as usize).saturating_sub(left_len + clock_len);

    left_spans.push(Span::raw(" ".repeat(padding)));
    left_spans.push(Span::styled(
        utc_clock,
        Style::default()
            .fg(app.theme.foreground_color())
            .add_modifier(Modifier::BOLD),
    ));

    let title = Line::from(left_spans);

    let paragraph = Paragraph::new(title).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );

    f.render_widget(paragraph, area);
}

/// One-row TX strip showing what's transmitting RIGHT NOW and what's
/// queued for an upcoming slot. Lightweight: reuses the coordinator's
/// `TxQueueUpdate` snapshot already in `App`.
fn render_tx_strip(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::new();

    // Non-deferred queued items that share the on-air slot are CONCURRENT
    // multi-TX streams (all keyed in the same 15s slot at different audio
    // frequencies), not a future-slot queue. Deferred items are genuinely
    // waiting for a later slot.
    let concurrent: Vec<&crate::app::TxQueueItem> = if app.tx_now_sending.is_some() {
        app.tx_queued.iter().filter(|it| !it.deferred).collect()
    } else {
        Vec::new()
    };
    let deferred: Vec<&crate::app::TxQueueItem> =
        app.tx_queued.iter().filter(|it| it.deferred).collect();

    match &app.tx_now_sending {
        Some(item) => {
            // LIVE TX — make it unmistakable. A bold red chip + bold frame
            // text dominate the strip for the full ~12.64s the message is
            // keyed (set at PTT-assert, cleared at PTT-release). The operator
            // repeatedly reported "I can't see what we're actually
            // transmitting WHILE we're transmitting it" — this is the fix.
            let now_count = 1 + concurrent.len();
            let label = if now_count > 1 {
                format!(" 🔴 TX NOW ×{} ", now_count)
            } else {
                " 🔴 TX NOW ".to_string()
            };
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
            let mut on_air: Vec<String> = vec![format!("{} @{:.0}Hz", item.text, item.freq_hz)];
            on_air.extend(
                concurrent
                    .iter()
                    .map(|it| format!("{} @{:.0}Hz", it.text, it.freq_hz)),
            );
            // The frame text itself, also white-on-red and bold so it reads as
            // a single dominant TX banner rather than a thin easily-missed
            // strip.
            spans.push(Span::styled(
                format!(" {} ", on_air.join("  |  ")),
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        None => {
            spans.push(Span::styled(
                "▶ NOW: (idle) ",
                Style::default().fg(app.theme.foreground_color()),
            ));
        }
    }

    spans.push(Span::raw("  "));

    if deferred.is_empty() {
        spans.push(Span::styled(
            "⋯ QUEUED: (none)",
            Style::default().fg(app.theme.foreground_color()),
        ));
    } else {
        spans.push(Span::styled(
            format!("⋯ QUEUED ({}): ", deferred.len()),
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD),
        ));
        // Show up to the first three deferred items so the strip stays 1 row.
        let shown: Vec<String> = deferred
            .iter()
            .take(3)
            .map(|it| format!("{} @{:.0}Hz → deferred 30s", it.text, it.freq_hz))
            .collect();
        let mut text = shown.join(" | ");
        if deferred.len() > 3 {
            text.push_str(&format!(" | +{} more", deferred.len() - 3));
        }
        spans.push(Span::styled(
            text,
            Style::default().fg(app.theme.warning_color()),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );
    f.render_widget(paragraph, area);
}

fn render_status_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let messages_count = app.decoded_messages.len();
    let dx_count = app.dx_stations.len();

    // Pipeline health indicators
    let (audio_indicator, dsp_indicator, decoder_indicator) = match &app.pipeline_health {
        Some(health) => {
            let audio = if health.audio_alive {
                Span::styled(
                    "AUD",
                    Style::default()
                        .fg(app.theme.success_color())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    "AUD",
                    Style::default()
                        .fg(app.theme.error_color())
                        .add_modifier(Modifier::BOLD),
                )
            };
            let dsp = if health.dsp_windows > 0 {
                Span::styled(
                    format!("DSP:{}", health.dsp_windows),
                    Style::default().fg(app.theme.success_color()),
                )
            } else {
                Span::styled("DSP:0", Style::default().fg(app.theme.error_color()))
            };
            let dec_label = if health.ft8lib_available {
                "FT8"
            } else {
                "FT8(native)"
            };
            let decoder = if health.total_decodes > 0 {
                Span::styled(
                    format!("{}:{}", dec_label, health.total_decodes),
                    Style::default().fg(app.theme.success_color()),
                )
            } else {
                Span::styled(
                    format!("{}:0", dec_label),
                    Style::default().fg(app.theme.warning_color()),
                )
            };
            (audio, dsp, decoder)
        }
        None => (
            Span::styled("AUD", Style::default().fg(app.theme.muted_color())),
            Span::styled("DSP", Style::default().fg(app.theme.muted_color())),
            Span::styled("FT8", Style::default().fg(app.theme.muted_color())),
        ),
    };

    let mut status_spans = vec![
        audio_indicator,
        Span::raw(" "),
        dsp_indicator,
        Span::raw(" "),
        decoder_indicator,
        Span::raw(" | "),
        Span::styled(
            format!("Level: {:.1}%", app.audio_level * 100.0),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("Msgs: {}", messages_count),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("DX: {}", dx_count),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        // While composing a free-text TX message, show the live input line
        // (with cursor) in bold instead of the transient status text so the
        // operator sees exactly what they're about to send.
        match app.compose_prompt() {
            Some(prompt) => Span::styled(
                prompt,
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ),
            None => Span::styled(
                app.status_message.clone(),
                Style::default().fg(app.theme.accent_color()),
            ),
        },
    ];

    // SWR — shown only while transmitting (swr_display() returns Some only for
    // a fresh reading, which is sampled solely during TX). Prepend it, bold and
    // color-graded by match quality, so it's unmistakable when keyed.
    if let Some(swr_text) = app.swr_display() {
        let color = match app.swr {
            Some(s) if s >= 3.0 => app.theme.error_color(),
            Some(s) if s >= 2.0 => app.theme.warning_color(),
            _ => app.theme.success_color(),
        };
        status_spans.insert(0, Span::raw(" | "));
        status_spans.insert(
            0,
            Span::styled(
                swr_text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        );
    }

    let status_line = Line::from(status_spans);

    // Always-visible key hints. These MUST match the real bindings in
    // tui_runner.rs (the help-overlay is the only other on-screen keymap):
    // band is `=`/`-` (not `+`), quit is `q` (not Ctrl+Q), `?` opens full help,
    // `d` opens the audio-device picker (the common "reclaim my output" action).
    let key_hint = |k: &'static str| {
        Span::styled(
            k,
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        )
    };
    let help_line = Line::from(vec![
        key_hint("Space"),
        Span::raw(":Call/Reply | "),
        key_hint("Arrows"),
        Span::raw(":TX | "),
        key_hint("=/-"),
        Span::raw(":Band | "),
        key_hint("g"),
        Span::raw(":TX-policy | "),
        key_hint("d"),
        Span::raw(":Audio | "),
        key_hint("?"),
        Span::raw(":Help | "),
        key_hint("q"),
        Span::raw(":Quit"),
    ]);

    // Split status bar into two lines
    let status_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let status_paragraph = Paragraph::new(status_line).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.foreground_color()),
    );

    let help_paragraph = Paragraph::new(help_line).style(
        Style::default()
            .bg(app.theme.background_color())
            .fg(app.theme.muted_color()),
    );

    f.render_widget(status_paragraph, status_chunks[0]);
    f.render_widget(help_paragraph, status_chunks[1]);

    // Border line
    let border_line = Line::from(vec![Span::raw("─".repeat(area.width as usize))]);
    let border_paragraph =
        Paragraph::new(border_line).style(Style::default().fg(app.theme.border_color()));
    f.render_widget(border_paragraph, status_chunks[2]);
}

fn render_waterfall(f: &mut Frame<'_>, area: Rect, app: &App) {
    // Collect recent decoded signal frequencies
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(30);
    let signal_freqs: Vec<f64> = app
        .decoded_messages
        .iter()
        .filter(|m| m.timestamp > cutoff)
        .map(|m| m.delta_freq as f64)
        .collect();

    // Build (freq, parity, timestamp) tuples for the occupancy strip from
    // recent decodes. Filter to last 60s; the widget further trims per-column
    // (±37.5 Hz of column center, and re-checks the 60s cutoff defensively).
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(60);
    let decoded_for_occupancy: Vec<(
        f64,
        pancetta_core::slot::SlotParity,
        chrono::DateTime<chrono::Utc>,
    )> = app
        .decoded_messages
        .iter()
        .filter(|m| m.timestamp >= cutoff)
        .filter_map(|m| m.slot_parity.map(|p| (m.delta_freq as f64, p, m.timestamp)))
        .collect();
    let tx_parity = app.resolve_tx_parity();

    // When something is actually on the air (autonomous/QSO TX), the green TX
    // cursor and its label should follow the LIVE TX frequency, not the manual
    // waterfall offset (the 1350→2300 visual bug). Fall back to the operator's
    // manual offset only when idle.
    let (cursor_offset, title) = match &app.tx_now_sending {
        Some(item) => (
            item.freq_hz,
            format!(" Waterfall [/]: TX {:.0} Hz (LIVE) ", item.freq_hz),
        ),
        None => (
            app.tx_frequency_offset,
            format!(" Waterfall [/]: TX {:.0} Hz ", app.tx_frequency_offset),
        ),
    };
    let waterfall_block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(app.theme.accent_color()),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border_color()));

    let waterfall = Waterfall::new(&app.waterfall_data)
        .block(waterfall_block)
        .tx_offset(cursor_offset)
        .signal_freqs(signal_freqs)
        .color_capability(app.color_capability)
        .decoded_for_occupancy(&decoded_for_occupancy)
        .tx_parity(tx_parity);
    f.render_widget(waterfall, area);
}

fn render_active_panel_highlight(f: &mut Frame<'_>, app: &App, panel_areas: &[Rect]) {
    let active_area = match app.active_panel {
        ActivePanel::BandActivity => panel_areas[0],
        ActivePanel::QsoStatus => panel_areas[1],
        ActivePanel::StationInfo => panel_areas[2],
        ActivePanel::Callers => panel_areas[3],
        ActivePanel::DxHunter => panel_areas[4],
        ActivePanel::TxPlacement => panel_areas[5],
    };

    // Draw a subtle highlight border around the active panel
    let highlight_block = Block::default().borders(Borders::ALL).border_style(
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD),
    );

    f.render_widget(highlight_block, active_area);
}

/// Create a styled block for panels
pub fn create_panel_block<'a>(title: &'a str, is_active: bool, app: &App) -> Block<'a> {
    let border_style = if is_active {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.border_color())
    };

    let title_style = if is_active {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.accent_color())
    };

    Block::default()
        .title(Span::styled(title, title_style))
        .borders(Borders::ALL)
        .border_style(border_style)
}

/// Render the Shift+F frequency-entry modal.
pub fn render_freq_modal(f: &mut Frame<'_>, area: Rect, m: &crate::app::FreqModalState) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 52.min(area.width.saturating_sub(4));
    let modal_height: u16 = 8.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let rx_focus = matches!(m.field, crate::app::FreqModalField::RxDial);
    let body = format!(
        " RX dial (MHz): {}{}\n TX split (MHz): {}{}\n   (blank = simplex)\n\n [Enter] apply   [Tab] field   [Esc] cancel",
        m.rx_buffer,
        if rx_focus { "_" } else { "" },
        m.tx_buffer,
        if !rx_focus { "_" } else { "" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Set Frequency ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Render the `o`-key TX-audio-offset modal.
///
/// One integer Hz field in [300, 2700]; blank Enter = Auto; Esc = cancel.
/// Mirrors the `render_freq_modal` layout and sizing.
pub fn render_offset_modal(f: &mut Frame<'_>, area: Rect, m: &crate::app::OffsetModalState) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 52.min(area.width.saturating_sub(4));
    let modal_height: u16 = 6.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let body = format!(
        " TX audio offset (Hz, 300–2700): {}_\n   blank = Auto (Tx=Rx)\n\n [Enter] apply   [Esc] cancel",
        m.buffer,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Set TX Offset ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Render the Shift+D diagnostics overlay: a scrollable, retained history of
/// `DiagnosticEvent`s (docs/observability-diagnostics-plan.md Layer 1) — the
/// "why did that happen" surface, as opposed to the single overwrite-prone
/// status line. Nearly full-screen (unlike the small input modals above)
/// since it's a scrollback list, not a form.
pub fn render_diagnostics_overlay(f: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let modal_width = area.width.saturating_sub(4);
    let modal_height = area.height.saturating_sub(4);
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " Diagnostics ({}/{}) — \u{2191}\u{2193}/jk scroll, Esc/D close ",
            app.diagnostic_events.len().min(app.diagnostics_scroll + 1),
            app.diagnostic_events.len()
        ))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    if app.diagnostic_events.is_empty() {
        f.render_widget(
            Paragraph::new(" No diagnostic events yet this session."),
            inner,
        );
        return;
    }

    // Show a window of rows ending at (or centered near) diagnostics_scroll,
    // newest-appropriate: one line per event, oldest-first within the window
    // (matches reading a log top-to-bottom).
    let visible_rows = inner.height as usize;
    let cursor = app
        .diagnostics_scroll
        .min(app.diagnostic_events.len().saturating_sub(1));
    let end = cursor + 1;
    let start = end.saturating_sub(visible_rows);

    let lines: Vec<Line> = app
        .diagnostic_events
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, ev)| {
            let color = match ev.level {
                pancetta_core::DiagnosticLevel::Info => Color::Gray,
                pancetta_core::DiagnosticLevel::Warn => Color::Yellow,
                pancetta_core::DiagnosticLevel::Error => Color::Red,
            };
            let who = ev
                .callsign
                .as_deref()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            let line = format!(
                "{} {:<14} {}{}",
                ev.ts.format("%H:%M:%S"),
                ev.target,
                ev.text,
                who
            );
            let style = if i == cursor {
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(color)
            };
            Line::from(Span::styled(line, style))
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

/// Render the required out-of-band acknowledgment modal.
pub fn render_out_of_band_modal(f: &mut Frame<'_>, area: Rect, tx_rf_hz: u64) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let modal_width: u16 = 60.min(area.width.saturating_sub(4));
    let modal_height: u16 = 7.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let body = format!(
        " TX {:.3} MHz is OUTSIDE the US ham bands.\n You are responsible for legal operation.\n\n [Enter] acknowledge",
        tx_rf_hz as f64 / 1e6,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" \u{26a0} Out of band ")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Helper function to get panel-specific colors
pub fn get_snr_color(snr: i32, theme: &crate::config::Theme) -> Color {
    match snr {
        snr if snr >= 0 => theme.success_color(),
        snr if snr >= -10 => theme.warning_color(),
        _ => theme.error_color(),
    }
}

/// Helper function to format distance
pub fn format_distance(distance: Option<f64>) -> String {
    match distance {
        Some(d) if d < 1000.0 => format!("{:.0} km", d),
        Some(d) => format!("{:.1}k km", d / 1000.0),
        None => "---".to_string(),
    }
}

/// Helper function to format bearing
pub fn format_bearing(bearing: Option<f64>) -> String {
    match bearing {
        Some(b) => format!("{:.0}°", b),
        None => "---".to_string(),
    }
}

/// Helper function to format time ago
pub fn format_time_ago(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration.num_seconds() < 60 {
        format!("{}s", duration.num_seconds())
    } else if duration.num_minutes() < 60 {
        format!("{}m", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h", duration.num_hours())
    } else {
        format!("{}d", duration.num_days())
    }
}

/// Title-bar chip label for the station's active operating mode.
///
/// Returns `Some(uppercased mode)` for any non-FT8 mode (e.g. `"FT4"`,
/// `"FT2"`) and `None` for FT8 — so the FT8 title bar stays chip-free and
/// byte-identical to its pre-mode-chip look. Matching is case-insensitive on
/// the configured `[rig].mode` string.
pub fn mode_chip_label(mode: &str) -> Option<String> {
    let m = mode.trim().to_uppercase();
    if m.is_empty() || m == "FT8" {
        None
    } else {
        Some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_chip_hidden_for_ft8() {
        assert_eq!(mode_chip_label("FT8"), None);
        assert_eq!(mode_chip_label("ft8"), None);
        assert_eq!(mode_chip_label("  FT8 "), None);
        assert_eq!(mode_chip_label(""), None);
    }

    #[test]
    fn mode_chip_shown_for_non_ft8() {
        assert_eq!(mode_chip_label("FT4"), Some("FT4".to_string()));
        assert_eq!(mode_chip_label("ft4"), Some("FT4".to_string()));
        assert_eq!(mode_chip_label("FT2"), Some("FT2".to_string()));
    }
}

#[cfg(test)]
mod view_render_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    async fn render_view_with_panel(
        view: crate::view::ActiveView,
        active_panel: crate::app::ActivePanel,
    ) -> ratatui::buffer::Buffer {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = view;
        // Pre-existing, out-of-scope-for-this-task quirk: `render_active_panel_highlight`
        // draws a titleless border directly over the active panel's own area,
        // which blanks that panel's title text (only its *border color*
        // changes; ratatui's Block widget overwrites the whole perimeter,
        // title included). That happens in Operate (which keeps the verbatim
        // highlight call) but not Monitor (which skips it per the brief).
        // Callers pick whichever panel they need NOT blanked for their
        // assertions and set `active_panel` accordingly.
        app.active_panel = active_panel;
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        term.backend().buffer().clone()
    }

    async fn render_view(view: crate::view::ActiveView) -> ratatui::buffer::Buffer {
        // Station Info is not asserted on by the (single-render) callers of
        // this helper, so it's the panel we sacrifice to the title-blanking
        // quirk above.
        render_view_with_panel(view, crate::app::ActivePanel::StationInfo).await
    }
    fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.contains(needle)
        })
    }

    /// Count of ROWS containing `needle` (not overlapping occurrences within
    /// a row — one row can only ever contribute a marker-row callsign once).
    /// Used to distinguish "the stream-marker row rendered this callsign"
    /// from "the pre-existing Active-QSOs banner (a completely separate
    /// renderer, row 1) already shows every active QSO's callsign" — both
    /// are always present when there are active QSOs, so a bare
    /// `buffer_contains` can't tell them apart.
    fn buffer_row_count_containing(buf: &ratatui::buffer::Buffer, needle: &str) -> usize {
        (0..buf.area.height)
            .filter(|&y| {
                let row: String = (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect();
                row.contains(needle)
            })
            .count()
    }

    #[tokio::test]
    async fn operate_view_shows_all_six_panels() {
        // Render #1: Station Info active — its own title is blanked by
        // `render_active_panel_highlight` (see the quirk note above), but
        // this proves the other five panels' titles render. Task 11 swapped
        // the waterfall for the TX-placement instrument here, so "TX
        // Placement" replaces the old waterfall-presence assertion.
        let buf = render_view(crate::view::ActiveView::Operate).await;
        for t in [
            "Band Activity",
            "QSO Status",
            "DX Hunter",
            "Callers",
            "TX Placement",
        ] {
            assert!(buffer_contains(&buf, t), "missing {t}");
        }

        // Render #2: a DIFFERENT panel (Band Activity, the app default)
        // active instead, so Station Info's own title is no longer blanked
        // and can be asserted — completing coverage of all 5 panels.
        let buf2 = render_view_with_panel(
            crate::view::ActiveView::Operate,
            crate::app::ActivePanel::BandActivity,
        )
        .await;
        assert!(
            buffer_contains(&buf2, "Station Info"),
            "missing Station Info"
        );
    }
    #[tokio::test]
    async fn monitor_view_drops_side_panels() {
        let buf = render_view(crate::view::ActiveView::Monitor).await;
        assert!(buffer_contains(&buf, "Band Activity"));
        assert!(!buffer_contains(&buf, "DX Hunter"));
        assert!(!buffer_contains(&buf, "Callers"));
        assert!(!buffer_contains(&buf, "QSO Status"));
        assert!(!buffer_contains(&buf, "Station Info"));
    }

    #[tokio::test]
    async fn hunt_view_shows_dx_hunter_and_band_activity_not_callers() {
        let buf = render_view(crate::view::ActiveView::Hunt).await;
        assert!(buffer_contains(&buf, "DX Hunter"));
        assert!(buffer_contains(&buf, "Band Activity"));
        assert!(buffer_contains(&buf, "TX Placement"));
        assert!(!buffer_contains(&buf, "Callers"));
    }

    #[tokio::test]
    async fn run_view_shows_callers_and_qso_status_not_dx_hunter() {
        let buf = render_view(crate::view::ActiveView::Run).await;
        assert!(buffer_contains(&buf, "Callers"));
        assert!(buffer_contains(&buf, "QSO Status"));
        assert!(buffer_contains(&buf, "TX Placement"));
        assert!(!buffer_contains(&buf, "DX Hunter"));
    }

    #[tokio::test]
    async fn zoom_renders_only_the_focused_panel() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_panel = crate::app::ActivePanel::DxHunter;
        app.zoomed = true;
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(buffer_contains(&buf, "DX Hunter"));
        assert!(!buffer_contains(&buf, "Band Activity"));
    }

    /// Minimal `ActiveQsoBanner` fixture for TX-placement stream-marker
    /// tests. Mirrors `app.rs`'s private `fixture_banner` test helper
    /// (not visible across the module boundary), trimmed to the fields
    /// these tests actually vary. Defaults `frequency_hz` to 1234.0 like
    /// its `app.rs` counterpart.
    fn fixture_banner(call: &str) -> crate::app::ActiveQsoBanner {
        crate::app::ActiveQsoBanner {
            their_callsign: call.into(),
            state: "wait rpt".into(),
            started_at: chrono::Utc::now(),
            frequency_hz: 1234.0,
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
            initiated_by: "Manual".into(),
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

    fn fixture_placement_view() -> crate::app::PlacementView {
        crate::app::PlacementView {
            slices: vec![
                crate::app::PlacementSlice {
                    offset_hz: 1480.0,
                    score: 98.0,
                    clear_first: true,
                    clear_second: true,
                },
                crate::app::PlacementSlice {
                    offset_hz: 920.0,
                    score: 91.0,
                    clear_first: true,
                    clear_second: true,
                },
                crate::app::PlacementSlice {
                    offset_hz: 310.0,
                    score: 71.0,
                    clear_first: false,
                    clear_second: true,
                },
            ],
            openness: vec![3; 96],
            bin_hz: 25.0,
            range: (200.0, 2600.0),
            received_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn tx_placement_shows_best_row_and_stream_marker_not_old_waterfall() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        // Pin Operate explicitly (App::new loads the OPERATOR's real
        // persisted `~/.pancetta/tui_state.json` view, same footgun
        // `render_view_with_panel` above already guards against).
        app.active_view = crate::view::ActiveView::Operate;
        app.apply_placement(fixture_placement_view());
        let mut b = fixture_banner("JA1ABC");
        b.frequency_hz = 1650.0;
        app.active_qsos.push(b);

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();

        assert!(
            buffer_contains(&buf, "\u{2460} 1480"),
            "missing BEST row rank ① 1480"
        );
        assert!(buffer_contains(&buf, "E+O"), "missing E+O coverage label");
        // JA1ABC legitimately appears TWICE: once in the pre-existing
        // Active-QSOs banner (row 1, unrelated to this task) and once as
        // the TX-placement stream-marker label this task adds — a bare
        // `buffer_contains` can't distinguish "the instrument rendered it"
        // from "the banner already did", so require 2 rows.
        assert_eq!(
            buffer_row_count_containing(&buf, "JA1ABC"),
            2,
            "expected JA1ABC on both the Active-QSOs banner row AND the TX-placement stream-marker row"
        );
        assert!(
            !buffer_contains(&buf, "\u{25bc}"),
            "old waterfall decode ticks (▼) should be gone from Operate"
        );
    }

    #[tokio::test]
    async fn tx_placement_zero_streams_renders_without_panic() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = crate::view::ActiveView::Operate;
        app.apply_placement(fixture_placement_view());
        assert!(app.active_qsos.is_empty());

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();

        // No panic, and the instrument itself still renders.
        assert!(buffer_contains(&buf, "TX Placement"));
    }

    #[tokio::test]
    async fn tx_placement_three_streams_shows_three_distinct_labels() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = crate::view::ActiveView::Operate;
        app.apply_placement(fixture_placement_view());
        let mut a = fixture_banner("K1AAA");
        a.frequency_hz = 400.0;
        let mut b = fixture_banner("K2BBB");
        b.frequency_hz = 1200.0;
        let mut c = fixture_banner("K3CCC");
        c.frequency_hz = 2200.0;
        app.active_qsos.push(a);
        app.active_qsos.push(b);
        app.active_qsos.push(c);

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();

        // Each callsign should appear on 2 rows: the Active-QSOs banner
        // (row 1, pre-existing) AND the TX-placement stream-marker row
        // this task adds (see the comment on the ①-slice test above for
        // why a bare `buffer_contains` isn't a strong enough check here).
        for call in ["K1AAA", "K2BBB", "K3CCC"] {
            assert_eq!(
                buffer_row_count_containing(&buf, call),
                2,
                "expected {call} on both the Active-QSOs banner row AND the TX-placement stream-marker row"
            );
        }
    }
}
