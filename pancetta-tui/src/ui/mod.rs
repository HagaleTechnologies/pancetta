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
pub mod station_card;
pub mod station_info;
pub mod tx_placement;

use active_qsos::render_active_qsos;
use band_activity::render_band_activity;
use callers::render_callers;
use dx_hunter::render_dx_hunter;
use qso_status::render_qso_status;
use station_card::render_station_card;
use tx_placement::{render_placement_zoom, render_tx_placement};

/// Minimum terminal size `draw()` will lay out panels for; below this it
/// shows a "resize the window" prompt instead. `hit_test` checks the same
/// floor so a click on that degraded screen (no panels actually visible)
/// never resolves to a phantom panel rect.
const MIN_TERMINAL_WIDTH: u16 = 80;
const MIN_TERMINAL_HEIGHT: u16 = 20;

// Task 18: `ActivePanel::StationInfo` was removed and `render_station_info`
// is no longer called from any layout function — the station card
// (`render_station_card`, above) took its slot in `layout_operate` (and was
// added below DX Hunter in `layout_hunt` when there's room). `ui/station_info.rs`
// is kept intact (not deleted, per the task brief) but is now unrouted; its
// module is still declared above so it continues to build and its own unit
// tests (grid/distance/bearing math) still run.

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
    if size.width < MIN_TERMINAL_WIDTH || size.height < MIN_TERMINAL_HEIGHT {
        let msg = vec![
            Line::from(Span::styled(
                "Terminal too small",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "Have {}x{}, need at least {}x{}.",
                size.width, size.height, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT
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
    let chunks = main_layout_chunks(size);

    // Render title bar
    render_title_bar(f, chunks[0], app);

    // Panel zoom (`z`): the focused panel fills the whole content area,
    // bypassing the active view's grid entirely. Orthogonal to the 4-view
    // system — checked before the view dispatch so it applies identically
    // regardless of `app.active_view`.
    if app.zoomed {
        let (banner, panel) = compute_zoom_rects(chunks[1]);
        render_active_qsos(f, banner, app);
        render_zoomed_panel(f, panel, app)?;
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

/// The top-level vertical split every frame renders from: title bar, main
/// content, TX strip, status bar. Pulled out of `draw()` (Task 19) so the
/// mouse hit-testing path (`App::handle_mouse_event`, which has no `Frame`
/// to render into) can recover the exact same main-content `Rect` the
/// renderer used via `content_area`, rather than re-deriving the split by
/// hand and risking drift.
fn main_layout_chunks(size: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Title bar (incl. bold TX-policy banner)
            Constraint::Min(1),    // Main content
            Constraint::Length(1), // TX queue / now-sending strip
            Constraint::Length(2), // Status bar (status line + help line)
        ])
        .split(size)
}

/// Split zoomed content into the persistent active-QSO banner and the focused
/// panel. Both drawing and hit testing use this helper so their geometry cannot
/// drift apart.
fn compute_zoom_rects(content_area: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(content_area);
    (rows[0], rows[1])
}

/// The main content `Rect` (`chunks[1]` from `draw()`) for a full-terminal
/// `size` — title bar, TX strip, and status bar stripped off. This is the
/// `area` that `view_rects`/`hit_test` expect; a click at absolute
/// terminal coordinates only needs to be compared against panel rects
/// computed from THIS rect, not the full terminal.
pub fn content_area(size: Rect) -> Rect {
    main_layout_chunks(size)[1]
}

/// Renders the focused panel (`app.active_panel`) full-screen in `area`,
/// bypassing whichever `ActiveView` grid is normally in effect. Calls the
/// SAME `render_*` function each view's grid layout uses, just with the
/// whole content rect instead of a sliced-down chunk — EXCEPT
/// `TxPlacement` (Task 13), which zooms into a genuinely different,
/// more-detailed top-10 table view (`render_placement_zoom`) rather than
/// the compact BEST-row instrument rendered bigger. Covers all 5
/// `ActivePanel` variants. (Task 18 removed `StationInfo` from the panel
/// cycle — the station card that replaced it has no `ActivePanel` variant
/// of its own, so it has no zoom entry here either; it's never the focused
/// panel.)
fn render_zoomed_panel(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    match app.active_panel {
        ActivePanel::BandActivity => render_band_activity(f, area, app)?,
        ActivePanel::QsoStatus => render_qso_status(f, area, app)?,
        ActivePanel::Callers => render_callers(f, area, app)?,
        ActivePanel::DxHunter => render_dx_hunter(f, area, app)?,
        // TxPlacement is the one panel whose zoom is NOT just "the same
        // renderer, bigger" (Task 13) — it swaps to a full-screen top-10
        // ranked table with columns the compact 5-row instrument can't fit.
        ActivePanel::TxPlacement => render_placement_zoom(f, area, app)?,
    }
    Ok(())
}

// === Task 19: shared pure rect maps ========================================
//
// Each view's `Layout::split` chain used to live only inside its
// `layout_*` render function (accumulated piecemeal across Tasks 5, 6, 11,
// 18). That meant a mouse click had no reliable way to know which panel a
// screen coordinate belonged to short of re-deriving the same split chain a
// second time — and any future drift between the two copies would silently
// break clicking without a compile error.
//
// The `compute_*_rects` functions below are now the ONE place each view's
// split chain lives. Both the `layout_*` render functions (which place
// every widget) and `view_rects`/`hit_test` (used by
// `App::handle_mouse_event`) call them, so a click always lands on exactly
// the boundary the renderer drew. The `compute_*_rects` functions return
// every rect a layout needs (including non-`ActivePanel` slots like the
// active-QSO banner and the station card); `view_rects` below filters that
// down to just the navigable `(ActivePanel, Rect)` pairs.

/// All rects the Operate view's layout needs.
struct OperateRects {
    banner: Rect,
    tx_placement: Rect,
    band_activity: Rect,
    qso_status: Rect,
    station_card: Rect,
    dx_hunter: Rect,
    callers: Rect,
}

/// Split chain extracted verbatim from the pre-Task-19 `layout_operate`
/// (itself extracted verbatim from `draw()` in Task 5, then given its
/// current top row in Task 11 and its station-card slot in Task 18).
fn compute_operate_rects(content_area: Rect) -> OperateRects {
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

    // Right column: the station card (Task 18 — took over Station Info's
    // slot; a static summary of the global focus, not a navigable panel) on
    // top, DX Hunter (moved up) in the middle, Callers on the bottom —
    // aligned with QSO Status across the gutter.
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20), // Station card
            Constraint::Percentage(40), // DX Hunter
            Constraint::Percentage(40), // Callers (bottom; across from QSO Status)
        ])
        .split(lower[1]);

    OperateRects {
        banner: content[0],
        tx_placement: content[1],
        band_activity: left_chunks[0],
        qso_status: left_chunks[1],
        station_card: right_chunks[0],
        dx_hunter: right_chunks[1],
        callers: right_chunks[2],
    }
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
/// Constraint note). Task 19 pulled the actual `Layout::split` chain out
/// into `compute_operate_rects` (this function now just renders from it) —
/// the rects themselves are unchanged, so this stays byte-identical too.
fn layout_operate(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let r = compute_operate_rects(content_area);

    // Render panels
    render_active_qsos(f, r.banner, app);
    render_tx_placement(f, r.tx_placement, app)?;
    render_band_activity(f, r.band_activity, app)?;
    render_qso_status(f, r.qso_status, app)?;
    render_station_card(f, r.station_card, app)?;
    render_dx_hunter(f, r.dx_hunter, app)?;
    render_callers(f, r.callers, app)?;

    Ok(())
}

/// All rects the Monitor view's layout needs.
struct MonitorRects {
    banner: Rect,
    waterfall: Rect,
    band_activity: Rect,
}

fn compute_monitor_rects(content_area: Rect) -> MonitorRects {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Percentage(60), // Big waterfall (full width)
            Constraint::Min(1),         // Band Activity (full width)
        ])
        .split(content_area);

    MonitorRects {
        banner: content[0],
        waterfall: content[1],
        band_activity: content[2],
    }
}

/// Monitor-view content layout: a vertical stack — full-width active-QSO
/// banner, a big waterfall, and full-width Band Activity — with no side
/// panels (QSO Status / station card / DX Hunter / Callers). Meant for a
/// glance-and-walk-away big-picture view.
///
fn layout_monitor(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let r = compute_monitor_rects(content_area);

    render_active_qsos(f, r.banner, app);
    render_waterfall(f, r.waterfall, app);
    render_band_activity(f, r.band_activity, app)?;

    Ok(())
}

/// All rects the Hunt view's layout needs. `station_card` is `None` on a
/// short terminal (see `compute_hunt_rects`).
struct HuntRects {
    banner: Rect,
    tx_placement: Rect,
    dx_hunter: Rect,
    station_card: Option<Rect>,
    band_activity: Rect,
    qso_status: Rect,
}

fn compute_hunt_rects(content_area: Rect) -> HuntRects {
    const STATION_CARD_HEIGHT: u16 = 5;
    // Threshold picked so the `draw()` resize-guard floor (MIN_H=20 →
    // content_area ~15 rows) never shows the card, while a normally-sized
    // terminal (TestBackend's 120x40 fixture → content_area 35 rows) does.
    const STATION_CARD_MIN_CONTENT_HEIGHT: u16 = 30;
    let show_card = content_area.height >= STATION_CARD_MIN_CONTENT_HEIGHT;

    let mut constraints = vec![
        Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
        Constraint::Length(7),      // TX Placement (full width)
        Constraint::Percentage(45), // DX Hunter (full width)
    ];
    if show_card {
        constraints.push(Constraint::Length(STATION_CARD_HEIGHT)); // Station card
    }
    constraints.push(Constraint::Percentage(25)); // Band Activity (full width, CQs-only)
    constraints.push(Constraint::Min(5)); // QSO Status

    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(content_area);

    let mut next_idx = 3;
    let station_card = if show_card {
        let r = content[next_idx];
        next_idx += 1;
        Some(r)
    } else {
        None
    };
    let band_activity = content[next_idx];
    let qso_status = content[next_idx + 1];

    HuntRects {
        banner: content[0],
        tx_placement: content[1],
        dx_hunter: content[2],
        station_card,
        band_activity,
        qso_status,
    }
}

/// Hunt-view content layout: DX Hunter gets top billing (full width) for
/// picking a rare/needed station to chase, the TX-placement instrument sits
/// underneath (per-stream markers matter here too — Task 11), the station
/// card (Task 18) drops in below DX Hunter when there's room, Band Activity
/// shows the narrowed CQs-only feed, and QSO Status anchors the bottom so
/// the operator can track an in-progress call. No Callers, no energy
/// waterfall (that stays in Monitor only) — this view is about hunting, not
/// answering.
///
/// Task 6 never gave Hunt a waterfall row to "replace", so the placement
/// strip is new real estate here rather than a swap; DX Hunter (the
/// dominant table per the design spec) keeps its 45% share, and Band
/// Activity's share shrinks (35%→25%) to make room instead.
///
/// The station card is genuinely conditional (per the Task 17 brief: "below
/// DX Hunter if ≥4 rows spare") — a short terminal (near the `draw()`
/// MIN_W/MIN_H resize-guard floor, whose content area is ~15 rows) keeps the
/// pre-card layout untouched rather than starving DX Hunter/Band
/// Activity/QSO Status for a card that would barely fit; a taller terminal
/// gets the extra 5-row block (3 content lines + 2 border, matching
/// `render_station_card`'s Operate-view sizing).
fn layout_hunt(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let r = compute_hunt_rects(content_area);

    render_active_qsos(f, r.banner, app);
    render_tx_placement(f, r.tx_placement, app)?;
    render_dx_hunter(f, r.dx_hunter, app)?;
    if let Some(card) = r.station_card {
        render_station_card(f, card, app)?;
    }
    render_band_activity(f, r.band_activity, app)?;
    render_qso_status(f, r.qso_status, app)?;

    Ok(())
}

/// All rects the Run view's layout needs.
struct RunRects {
    banner: Rect,
    tx_placement: Rect,
    callers: Rect,
    qso_status: Rect,
}

fn compute_run_rects(content_area: Rect) -> RunRects {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Active-QSOs banner (full width, 1 row)
            Constraint::Length(7),      // TX Placement (full width)
            Constraint::Percentage(50), // Callers (full width)
            Constraint::Min(5),         // QSO Status (multi-table "Active QSOs" mode)
        ])
        .split(content_area);

    RunRects {
        banner: content[0],
        tx_placement: content[1],
        callers: content[2],
        qso_status: content[3],
    }
}

/// Run-view content layout: Callers gets top billing (full width) for
/// working stations calling us, the TX-placement instrument sits underneath
/// (its per-stream TX markers matter most here — serving a pileup), and QSO
/// Status anchors the bottom in its existing multi-table ("Active QSOs")
/// mode so the operator can track several concurrent exchanges at once. No
/// DX Hunter, no Band Activity, no station card, no energy waterfall — this
/// view is about answering, not hunting.
///
/// Task 6 never gave Run a waterfall row to "replace" either (see
/// `layout_hunt`'s doc comment); Callers (the dominant table) keeps its 50%
/// share, and QSO Status's `Min(5)` absorbs the new fixed-height row same as
/// it already absorbed the pre-existing remainder.
fn layout_run(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()> {
    let r = compute_run_rects(content_area);

    render_active_qsos(f, r.banner, app);
    render_tx_placement(f, r.tx_placement, app)?;
    render_callers(f, r.callers, app)?;
    render_qso_status(f, r.qso_status, app)?;

    Ok(())
}

/// The rect map for a given view/zoom state — the single source of truth
/// shared by the `layout_*` render functions (via the `compute_*_rects`
/// helpers above, which this calls) and `hit_test` (mouse click routing,
/// Task 19). Returns one `(ActivePanel, Rect)` pair per NAVIGABLE panel the
/// view currently shows — non-panel content (the active-QSO banner, the
/// energy waterfall, the station card) is never included, since a click on
/// it has no `ActivePanel` to focus.
///
/// `active_panel` is only consulted when `zoomed` — it's not part of the
/// literal brief signature (`view_rects(view, zoomed, area)`), but
/// `render_zoomed_panel`'s dispatch (which this mirrors) fills the WHOLE
/// content area with whichever panel `app.active_panel` currently names,
/// independent of `view`; there is no way to reproduce that pure-function
/// side without knowing which panel is zoomed. Every non-zoomed call site
/// in this task passes `app.active_panel` too, so it's always in scope at
/// the caller.
pub fn view_rects(
    view: crate::view::ActiveView,
    zoomed: bool,
    active_panel: ActivePanel,
    area: Rect,
) -> Vec<(ActivePanel, Rect)> {
    if zoomed {
        // Mirrors `render_zoomed_panel`: the focused panel fills the content
        // area below the reserved active-QSO banner, regardless of view.
        return vec![(active_panel, compute_zoom_rects(area).1)];
    }
    match view {
        crate::view::ActiveView::Operate => {
            let r = compute_operate_rects(area);
            vec![
                (ActivePanel::BandActivity, r.band_activity),
                (ActivePanel::QsoStatus, r.qso_status),
                (ActivePanel::Callers, r.callers),
                (ActivePanel::DxHunter, r.dx_hunter),
                (ActivePanel::TxPlacement, r.tx_placement),
            ]
        }
        crate::view::ActiveView::Monitor => {
            let r = compute_monitor_rects(area);
            vec![(ActivePanel::BandActivity, r.band_activity)]
        }
        crate::view::ActiveView::Hunt => {
            let r = compute_hunt_rects(area);
            vec![
                (ActivePanel::TxPlacement, r.tx_placement),
                (ActivePanel::DxHunter, r.dx_hunter),
                (ActivePanel::BandActivity, r.band_activity),
                (ActivePanel::QsoStatus, r.qso_status),
            ]
        }
        crate::view::ActiveView::Run => {
            let r = compute_run_rects(area);
            vec![
                (ActivePanel::TxPlacement, r.tx_placement),
                (ActivePanel::Callers, r.callers),
                (ActivePanel::QsoStatus, r.qso_status),
            ]
        }
    }
}

/// Row offset (border + header rows) between a panel's rect and its first
/// data row, for the purposes of mapping a click to a row index. Table
/// panels (Band Activity / DX Hunter / Callers / QSO Status's multi-QSO
/// table) draw a 1-cell border plus a 1-row header before their first data
/// row (verified against each panel's `Table::new(...).header(...)`
/// construction). TxPlacement is not a table — it's a fixed 5-row
/// instrument with a border but no header row, so row 0 (right after the
/// border) IS the first real row (the openness strip).
fn panel_row_offset(panel: ActivePanel) -> u16 {
    match panel {
        ActivePanel::TxPlacement => 1,
        ActivePanel::BandActivity
        | ActivePanel::QsoStatus
        | ActivePanel::Callers
        | ActivePanel::DxHunter => 2,
    }
}

/// Map a click at absolute terminal coordinates `(x, y)` to the panel it
/// landed in and the row within that panel's DATA rows (border/header
/// rows already subtracted — see `panel_row_offset`). `None` when the
/// point falls outside every navigable panel's rect (e.g. the active-QSO
/// banner, the TX strip, or the status bar — none of which are part of
/// `view_rects`'s output).
///
/// Pure geometry — computed from the SAME `view_rects` the renderer's
/// `layout_*` functions draw from, so a click always lands on the panel
/// boundary the operator actually sees on screen.
pub fn hit_test(
    view: crate::view::ActiveView,
    zoomed: bool,
    active_panel: ActivePanel,
    area: Rect,
    x: u16,
    y: u16,
) -> Option<(ActivePanel, u16)> {
    // `area` here is the CONTENT area (title bar/TX strip/status bar already
    // stripped off, see `content_area`), which is always a few rows shorter
    // than the full terminal `draw()`'s resize guard checks against — so the
    // floor below is the content area's size at the exact MIN_TERMINAL_*
    // boundary, not those constants themselves (comparing directly against
    // them would reject clicks on plenty of valid, non-degraded terminals
    // right at the floor).
    let min_content = content_area(Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT));
    if area.width < min_content.width || area.height < min_content.height {
        return None;
    }
    for (panel, rect) in view_rects(view, zoomed, active_panel, area) {
        if x < rect.x || x >= rect.x + rect.width || y < rect.y || y >= rect.y + rect.height {
            continue;
        }
        let local_row = y - rect.y;
        let row = local_row.saturating_sub(panel_row_offset(panel));
        return Some((panel, row));
    }
    None
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
    ];
    // #171: our own DXCC entity is inserted retroactively, right after this
    // index, only if it fits — see the width check near the padding
    // calculation below (the title bar has no wrap/overflow handling, so an
    // unconditional entity span can silently clip a later chip, e.g. the
    // QSOs counter, once several other chips are already active).
    let entity_insert_idx = left_spans.len();
    left_spans.extend([
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
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
    ]);

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
    // (pancetta picks/adjusts). Task 20e: informational, not urgent — no
    // background; accent fg + bold like the other informational chips below.
    let freq_text = match app.tx_freq_mode {
        pancetta_core::TxFreqMode::Hold => " FREQ: HOLD ".to_string(),
        pancetta_core::TxFreqMode::Auto => " FREQ: AUTO ".to_string(),
    };
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        freq_text,
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));

    // Decode-effort chip (decoder-speed-overhaul Task 15): the live preset
    // + the most recently completed decode window's wall-time, with a
    // trailing scissors mark when that window ran out of its budget before
    // finishing optional work. `app.decode_effort` is authoritative-from-
    // frame-1 (the coordinator seeds it at startup, not just on the
    // operator's first `e` press); `pipeline_health` is `None` only for the
    // first ~2s before the first health tick, in which case the elapsed
    // reads as 0ms (never exhausted) rather than hiding the chip.
    let (decode_elapsed_ms, decode_exhausted) = app
        .pipeline_health
        .as_ref()
        .map(|h| (h.last_decode_elapsed_ms, h.last_decode_budget_exhausted))
        .unwrap_or((0, false));
    let decode_text = format!(
        " DECODE: {} {}ms{} ",
        app.decode_effort,
        decode_elapsed_ms,
        if decode_exhausted { " ✂" } else { "" }
    );
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        decode_text,
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));

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
    // Task 20e: informational — no background.
    if app.split_tx_hz != 0 {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" SPLIT TX {:.3} ", app.split_tx_hz as f64 / 1e6),
            Style::default()
                .fg(app.theme.accent_color())
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
    // Task 20e: informational — no background.
    if let Some(offset_hz) = app.tx_offset_hold_hz {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" TX off: {} (HOLD) ", offset_hz),
            Style::default()
                .fg(app.theme.accent_color())
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

    // Session QSO counter (Task 20d): shown only once the operator has
    // actually completed one, right before the clock — quiet at session
    // start, unmissable once it matters.
    if app.session_completed > 0 {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!("QSOs: {} ", app.session_completed),
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Calculate padding to right-align the UTC clock
    let mut left_len: usize = left_spans.iter().map(|s| s.width()).sum();
    let clock_len = utc_clock.len();

    // #171: only insert our own entity if it actually fits alongside every
    // other chip already committed to this row — inserted here (not up
    // front) because whether it fits depends on which OTHER chips ended up
    // active (TX policy, FREQ, DECODE, QSOs counter, ...), known only now.
    if let Some(entity) = app.station_info.entity_name.as_deref() {
        let entity_span = Span::styled(
            format!(" ({entity})"),
            Style::default().fg(app.theme.muted_color()),
        );
        let entity_width = entity_span.width();
        if left_len + entity_width + clock_len <= area.width as usize {
            left_spans.insert(entity_insert_idx, entity_span);
            left_len += entity_width;
        }
    }

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
    const TX_STRIP_MAX_ITEMS: usize = 3;
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
            let live = app.live_tx_assignments();
            if live.is_empty() {
                spans.push(Span::styled(
                    "▶ NOW: (idle) ",
                    Style::default().fg(app.theme.foreground_color()),
                ));
            } else {
                spans.push(Span::styled(
                    format!("▶ NEXT TX ({}): ", live.len()),
                    Style::default()
                        .fg(app.theme.accent_color())
                        .add_modifier(Modifier::BOLD),
                ));
                let mut text = live
                    .iter()
                    .take(TX_STRIP_MAX_ITEMS)
                    .map(|a| format!("{} @{:.0}Hz", a.callsign, a.offset_hz))
                    .collect::<Vec<_>>()
                    .join("  |  ");
                if live.len() > TX_STRIP_MAX_ITEMS {
                    text.push_str(&format!("  |  +{} more", live.len() - TX_STRIP_MAX_ITEMS));
                }
                spans.push(Span::styled(
                    text,
                    Style::default().fg(app.theme.foreground_color()),
                ));
            }
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
            .take(TX_STRIP_MAX_ITEMS)
            .map(|it| format!("{} @{:.0}Hz → deferred 30s", it.text, it.freq_hz))
            .collect();
        let mut text = shown.join(" | ");
        if deferred.len() > TX_STRIP_MAX_ITEMS {
            text.push_str(&format!(" | +{} more", deferred.len() - TX_STRIP_MAX_ITEMS));
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
        // Rig S-meter (Task 18 — demoted from the Station Info panel). Reuses
        // `App::signal_strength_status_bar`, which shares the exact same
        // `signal_strength_db`/`signal_strength_at` fields + staleness gate
        // as the pre-existing `s_meter_display` (just a compact
        // "S:{signed dB}" / "S:---" format, sized for this one-line bar).
        Span::styled(
            app.signal_strength_status_bar(),
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" | "),
        // Audio device short name — same source `station_info.rs` used
        // (`app.audio_device`, falling back to "Default").
        Span::styled(
            format!("Dev: {}", app.audio_device.as_deref().unwrap_or("Default")),
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

    // Split status bar into two lines (Task 20c: the third row — a purely
    // decorative "─" border with no information — was dropped, along with
    // the outer layout's matching `Length(3)` -> `Length(2)`).
    let status_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
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

/// Color a `Failed` row by how actionable/severe the reason is — mirrors the
/// Diagnostics overlay's level-based coloring, but keyed off
/// `QsoFailureReason` since terminal-QSO outcomes don't carry a
/// `DiagnosticLevel`. Red = a real defect (protocol/callsign/frequency
/// problem worth investigating); Yellow = propagation/timing, likely just
/// band conditions; Gray = benign/administrative (cancelled, superseded,
/// deduped).
fn recent_qso_failure_color(reason: &pancetta_qso::QsoFailureReason) -> Color {
    use pancetta_qso::QsoFailureReason as R;
    match reason {
        R::Timeout | R::SignalLost | R::StationQrt | R::SupervisorRestart => Color::Yellow,
        R::InvalidCallsign | R::FrequencyConflict | R::ProtocolError(_) => Color::Red,
        R::Duplicate | R::UserCancelled | R::Superseded => Color::Gray,
    }
}

/// Render the Shift+R Recent-QSOs panel: a scrollable, retained history of
/// terminal QSO outcomes (docs/observability-diagnostics-plan.md Layer 2) —
/// "what happened to my last N QSOs," as opposed to the Diagnostics
/// overlay's finer-grained "why did this specific thing happen." Mirrors
/// `render_diagnostics_overlay`'s layout/scroll/empty-state conventions
/// exactly; only the row formatting and color rule differ.
pub fn render_recent_qsos_panel(f: &mut Frame<'_>, area: Rect, app: &App) {
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
            " Recent QSOs ({}/{}) — \u{2191}\u{2193}/jk scroll, Esc/R close ",
            app.recent_qsos.len().min(app.recent_qsos_scroll + 1),
            app.recent_qsos.len()
        ))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    if app.recent_qsos.is_empty() {
        f.render_widget(
            Paragraph::new(" No completed QSOs yet this session."),
            inner,
        );
        return;
    }

    // Same windowing approach as the Diagnostics overlay: a page of rows
    // ending at (or centered near) the scroll cursor, oldest-first within
    // the window.
    let visible_rows = inner.height as usize;
    let cursor = app
        .recent_qsos_scroll
        .min(app.recent_qsos.len().saturating_sub(1));
    let end = cursor + 1;
    let start = end.saturating_sub(visible_rows);

    let lines: Vec<Line> = app
        .recent_qsos
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, qso)| {
            let color = match &qso.outcome {
                crate::app::QsoOutcome::Completed => Color::Green,
                crate::app::QsoOutcome::Failed(reason) => recent_qso_failure_color(reason),
            };
            // `brief_timeline`'s last line is the terminal summary the
            // coordinator already built ("Completed" / "Failed: <reason>") —
            // reuse it rather than re-deriving reason text here. Row format
            // per spec (docs/superpowers/plans/2026-07-25-observability-
            // recent-qsos-and-timeline.md): callsign, outcome + reason, freq.
            // "Final rung reached" / precise duration aren't in scope for
            // this payload — that's the fuller per-message `state_history`
            // Task 4 persists; `brief_timeline` is a short digest only.
            let summary = qso
                .brief_timeline
                .last()
                .map(String::as_str)
                .unwrap_or(&qso.last_state);
            let line = format!(
                "{} {:<10} {}  ({} Hz)",
                qso.ts.format("%H:%M:%S"),
                qso.callsign,
                summary,
                qso.freq_hz
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

/// Render the Shift+S station-health panel: a consolidated "is the station
/// healthy right now?" snapshot (docs/observability-diagnostics-plan.md
/// Layer 3), aggregating signals already computed elsewhere rather than
/// introducing new detection logic. Unlike `render_diagnostics_overlay`
/// (a scrollback), this is a point-in-time view — no scroll state.
pub fn render_health_panel(f: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let modal_width = area.width.saturating_sub(4).min(70);
    let modal_height = area.height.saturating_sub(4).min(16);
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Station Health — Esc/S close ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    fn dot(ok: bool) -> Span<'static> {
        if ok {
            Span::styled("●", Style::default().fg(Color::Green))
        } else {
            Span::styled("●", Style::default().fg(Color::Red))
        }
    }

    let mut lines: Vec<Line> = Vec::new();

    match &app.pipeline_health {
        Some(h) => {
            lines.push(Line::from(vec![
                dot(h.audio_alive),
                Span::raw(format!(
                    " Audio: {}  ({} DSP windows)",
                    if h.audio_alive { "alive" } else { "DEAD" },
                    h.dsp_windows
                )),
            ]));
            lines.push(Line::from(vec![
                dot(h.ft8lib_available),
                Span::raw(format!(
                    " Decoder: {}",
                    if h.ft8lib_available {
                        "ft8_lib (native)"
                    } else {
                        "STUB — not compiled in"
                    }
                )),
            ]));
            lines.push(Line::from(Span::raw(format!(
                "  Total decodes: {}   Last window: {}ms{}",
                h.total_decodes,
                h.last_decode_elapsed_ms,
                if h.last_decode_budget_exhausted {
                    " (budget exhausted)"
                } else {
                    ""
                }
            ))));
            lines.push(Line::from(Span::raw(format!(
                "  TX attempts: {}   TX defers: {}",
                h.tx_attempts, h.tx_defers
            ))));
            let panics_ok = h.decode_panic_count == 0 && h.wdt_panic_count == 0;
            lines.push(Line::from(vec![
                dot(panics_ok),
                Span::raw(format!(
                    " Panics: decode={} watchdog={}",
                    h.decode_panic_count, h.wdt_panic_count
                )),
            ]));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "  Waiting for first health tick (~2s after startup)...",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines.push(Line::from(""));

    let rig_ok = !matches!(app.rig_connected, crate::app::RigConnDisplay::PollingFailed);
    lines.push(Line::from(vec![
        dot(rig_ok),
        Span::raw(format!(" Rig: {:?}", app.rig_connected)),
    ]));
    lines.push(Line::from(vec![
        dot(app.tx_output_default),
        Span::raw(format!(
            " TX audio output: {}",
            if app.tx_output_default {
                "system default"
            } else {
                "NOT default — check device"
            }
        )),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::raw(format!(
        "  QSOs completed: {}   failed: {}   TX drops: {}",
        app.session_completed, app.session_failed, app.session_tx_drops
    ))));

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

#[cfg(test)]
mod view_render_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    async fn render_view(view: crate::view::ActiveView) -> ratatui::buffer::Buffer {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = view;
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
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
        // Task 11 swapped the waterfall for the TX-placement instrument
        // here, so "TX Placement" replaces the old waterfall-presence
        // assertion. Task 18: `StationInfo` is no longer an `ActivePanel`
        // variant; the station card that took its slot (title "Station")
        // has no active/inactive styling distinction to worry about.
        let buf = render_view(crate::view::ActiveView::Operate).await;
        for t in [
            "Band Activity",
            "QSO Status",
            "DX Hunter",
            "Callers",
            "TX Placement",
            "Station",
        ] {
            assert!(buffer_contains(&buf, t), "missing {t}");
        }
    }

    /// Task 18: the S-meter and audio-device spans that used to live only
    /// in the (now-unrouted) Station Info panel join the status bar.
    #[tokio::test]
    async fn status_bar_shows_s_meter_and_device_spans() {
        let buf = render_view(crate::view::ActiveView::Operate).await;
        assert!(
            buffer_contains(&buf, "S:---"),
            "missing S-meter placeholder span"
        );
        assert!(buffer_contains(&buf, "Dev:"), "missing device span");
    }

    /// Task 20d: the "QSOs: N" title-bar chip is hidden at session start
    /// (n=0) and shown once the session counter has ticked up.
    #[tokio::test]
    async fn title_bar_shows_session_qso_counter_only_after_first_completion() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();

        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        assert!(
            !buffer_contains(term.backend().buffer(), "QSOs:"),
            "counter chip must be hidden at n=0"
        );

        app.session_completed = 3;
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        assert!(
            buffer_contains(term.backend().buffer(), "QSOs: 3"),
            "counter chip must show the completed count"
        );
    }

    /// Task 20e: the informational title-bar chips (FREQ mode, SPLIT, TX
    /// offset, active-mode) render with NO background — accent fg + bold
    /// only — while the TX-policy banner keeps its colored background. Finds
    /// a cell in row 0 by its glyph and checks the style directly (rather
    /// than a substring scan) so we're asserting on the actual `Style`, not
    /// just presence of the text.
    #[tokio::test]
    async fn informational_chips_have_no_background_tx_policy_banner_keeps_its_own() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.tx_freq_mode = pancetta_core::TxFreqMode::Hold;
        app.split_tx_hz = 14_074_000;
        app.tx_offset_hold_hz = Some(920);
        app.station_info.mode = "FT4".to_string();

        let backend = TestBackend::new(160, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();

        // Row 0 is the title bar. Walk every cell and assert: any cell whose
        // fg is the theme's accent color carries NO background paint (the
        // chip's bg must equal the plain title-bar background), while at
        // least one cell still carries the TX-policy banner's Green bg (that
        // one is untouched by this task).
        let accent = app.theme.accent_color();
        let bg = app.theme.background_color();
        let mut found_accent_chip = false;
        let mut found_policy_banner = false;
        for x in 0..buf.area.width {
            let cell = &buf[(x, 0)];
            if cell.fg == accent && cell.symbol() != " " {
                found_accent_chip = true;
                assert_eq!(
                    cell.bg, bg,
                    "informational chip cell at x={x} must have no background (found {:?})",
                    cell.bg
                );
            }
            if cell.bg == ratatui::style::Color::Green {
                found_policy_banner = true;
            }
        }
        assert!(
            found_accent_chip,
            "expected at least one accent-fg chip cell"
        );
        assert!(
            found_policy_banner,
            "TX-policy banner (Green bg) must be untouched"
        );
    }

    #[tokio::test]
    async fn monitor_view_drops_side_panels() {
        let buf = render_view(crate::view::ActiveView::Monitor).await;
        assert!(buffer_contains(&buf, "Band Activity"));
        assert!(!buffer_contains(&buf, "DX Hunter"));
        assert!(!buffer_contains(&buf, "Callers"));
        assert!(!buffer_contains(&buf, "QSO Status"));
        // Neither the (removed) Station Info panel nor the station card that
        // replaced it are ever rendered in Monitor.
        assert!(!buffer_contains(&buf, "Station"));
    }

    #[tokio::test]
    async fn hunt_view_shows_dx_hunter_and_band_activity_not_callers() {
        let buf = render_view(crate::view::ActiveView::Hunt).await;
        assert!(buffer_contains(&buf, "DX Hunter"));
        assert!(buffer_contains(&buf, "Band Activity"));
        assert!(buffer_contains(&buf, "TX Placement"));
        assert!(!buffer_contains(&buf, "Callers"));
        // TestBackend's 120x40 fixture gives Hunt a 35-row content area,
        // comfortably over the `layout_hunt` STATION_CARD_MIN_CONTENT_HEIGHT
        // (30), so the station card (Task 18) should show up below DX Hunter.
        assert!(
            buffer_contains(&buf, "Station"),
            "station card should render below DX Hunter when there's room"
        );
    }

    /// `layout_hunt` only adds the station card when the terminal has room
    /// to spare (Task 17 brief: "below DX Hunter if ≥4 rows spare") — a
    /// short terminal keeps the pre-card layout so DX Hunter / Band Activity
    /// / QSO Status aren't starved, and (just as importantly) never panics
    /// trying to lay out a row that doesn't fit.
    #[tokio::test]
    async fn hunt_view_omits_station_card_on_a_short_terminal() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = crate::view::ActiveView::Hunt;
        // 80x24: above the draw() resize-guard floor (80x20) but well under
        // the station-card room threshold once title/strip/status-bar rows
        // are subtracted (content area = 24 - 1 - 1 - 3 = 19 rows).
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(buffer_contains(&buf, "DX Hunter"));
        assert!(
            !buffer_contains(&buf, "Station"),
            "station card should be omitted on a short terminal"
        );
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

    /// A canned 10-slice `PlacementView` for the Task-13 top-10 zoom-table
    /// test — distinct from `fixture_placement_view` (3 slices, used by the
    /// compact-instrument tests) because the zoom table needs a full top-10
    /// to prove all 10 rows render, not just the 5 the BEST row shows.
    fn fixture_placement_view_10() -> crate::app::PlacementView {
        let mut slices = Vec::new();
        for i in 0..10 {
            slices.push(crate::app::PlacementSlice {
                offset_hz: 1480.0 - (i as f64) * 10.0,
                score: 98.0 - i as f64,
                clear_first: true,
                clear_second: true,
            });
        }
        crate::app::PlacementView {
            slices,
            openness: vec![3; 96],
            bin_hz: 25.0,
            range: (200.0, 2600.0),
            received_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn zoom_tx_placement_shows_top_10_table() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = crate::view::ActiveView::Operate;
        app.active_panel = crate::app::ActivePanel::TxPlacement;
        app.zoomed = true;
        app.apply_placement(fixture_placement_view_10());

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();

        assert!(buffer_contains(&buf, "Freq"), "missing Freq header");
        assert!(
            buffer_contains(&buf, "\u{2460} 1480"),
            "missing rank-1 row ① 1480"
        );
        // All 10 slices should render as distinct rows — check a spread of
        // offsets across the fixture (first, middle, last).
        for needle in ["1480", "1430", "1390"] {
            assert!(
                buffer_contains(&buf, needle),
                "missing candidate offset {needle}"
            );
        }
    }

    #[tokio::test]
    async fn tx_placement_shows_best_row_and_stream_marker_not_old_waterfall() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        // Pin Operate explicitly (App::new loads the OPERATOR's real
        // persisted `~/.pancetta/tui_state.json` view, same footgun
        // `render_view` above already guards against).
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
            3,
            "expected JA1ABC on the Active-QSOs banner, TX-placement marker, and NEXT TX rows"
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
                3,
                "expected {call} on the Active-QSOs banner, TX-placement marker, and NEXT TX rows"
            );
        }
    }

    /// Task 5: `render_health_panel` (Shift+S station-health panel) must
    /// not panic before the first health tick (`pipeline_health` is `None`
    /// for ~2s after startup) nor once populated, and must render the
    /// aggregated signals it's meant to surface.
    #[tokio::test]
    async fn render_health_panel_smoke_test() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();

        // No pipeline_health yet — must not panic, and shows the
        // waiting-for-first-tick message.
        term.draw(|f| render_health_panel(f, f.area(), &app))
            .unwrap();
        let buf = term.backend().buffer().clone();
        assert!(buffer_contains(&buf, "Station Health"));
        assert!(buffer_contains(&buf, "Waiting for first health tick"));

        // Populated pipeline_health plus session counters — must not panic,
        // and shows the aggregated signals.
        app.pipeline_health = Some(crate::app::PipelineHealth {
            audio_alive: true,
            dsp_windows: 42,
            last_rms: 0.01,
            ft8lib_available: true,
            total_decodes: 7,
            last_decode_elapsed_ms: 120,
            last_decode_budget_exhausted: false,
            tx_attempts: 3,
            tx_defers: 1,
            decode_panic_count: 0,
            wdt_panic_count: 0,
        });
        app.session_completed = 2;
        app.session_failed = 1;
        app.session_tx_drops = 0;
        term.draw(|f| render_health_panel(f, f.area(), &app))
            .unwrap();
        let buf2 = term.backend().buffer().clone();
        assert!(buffer_contains(&buf2, "Audio: alive"));
        assert!(buffer_contains(&buf2, "ft8_lib (native)"));
        assert!(buffer_contains(&buf2, "Rig:"));
        assert!(buffer_contains(&buf2, "QSOs completed: 2"));

        // Also must not panic at tiny/degenerate sizes (mirrors the
        // device-selection-modal underflow-guard test in tui_runner.rs).
        for (w, h) in [(1u16, 1u16), (0, 0), (10, 2), (19, 5)] {
            let backend = TestBackend::new(w.max(1), h.max(1));
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| render_health_panel(f, f.area(), &app))
                .unwrap();
        }
    }

    /// Task 3: `render_recent_qsos_panel` (Shift+R Recent-QSOs panel) must
    /// not panic on an empty ring, and must render a mixed
    /// Completed/Failed outcome stream with per-outcome coloring — green
    /// for Completed, red/yellow/gray by failure reason for Failed.
    #[tokio::test]
    async fn render_recent_qsos_panel_smoke_test() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();

        // Empty ring — must not panic, and shows the empty-state message.
        term.draw(|f| render_recent_qsos_panel(f, f.area(), &app))
            .unwrap();
        let buf = term.backend().buffer().clone();
        assert!(buffer_contains(&buf, "Recent QSOs"));
        assert!(buffer_contains(&buf, "No completed QSOs yet this session."));

        // SupervisorRestart rides the same Yellow bucket as Timeout — a
        // system-side interruption, not an operator or protocol error.
        assert_eq!(
            recent_qso_failure_color(&pancetta_qso::QsoFailureReason::SupervisorRestart),
            Color::Yellow
        );

        // A mixed stream: one Completed, and Faileds spanning all three
        // color buckets (Timeout=yellow, InvalidCallsign=red,
        // UserCancelled=gray).
        app.push_recent_qso_outcome(crate::app::RecentQsoOutcome {
            callsign: "K1ABC".to_string(),
            outcome: crate::app::QsoOutcome::Completed,
            last_state: "Completed".to_string(),
            freq_hz: 1500,
            ts: chrono::Utc::now(),
            brief_timeline: vec![
                "K1ABC started at 00:00:00".to_string(),
                "Completed".to_string(),
            ],
        });
        app.push_recent_qso_outcome(crate::app::RecentQsoOutcome {
            callsign: "KJ5NJF".to_string(),
            outcome: crate::app::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout),
            last_state: "Failed".to_string(),
            freq_hz: 1832,
            ts: chrono::Utc::now(),
            brief_timeline: vec![
                "KJ5NJF started at 00:00:05".to_string(),
                "Failed: watchdog timeout".to_string(),
            ],
        });
        app.push_recent_qso_outcome(crate::app::RecentQsoOutcome {
            callsign: "W2XYZ".to_string(),
            outcome: crate::app::QsoOutcome::Failed(
                pancetta_qso::QsoFailureReason::InvalidCallsign,
            ),
            last_state: "Failed".to_string(),
            freq_hz: 2100,
            ts: chrono::Utc::now(),
            brief_timeline: vec!["Failed: invalid callsign".to_string()],
        });
        app.push_recent_qso_outcome(crate::app::RecentQsoOutcome {
            callsign: "N3TEST".to_string(),
            outcome: crate::app::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::UserCancelled),
            last_state: "Failed".to_string(),
            freq_hz: 800,
            ts: chrono::Utc::now(),
            brief_timeline: vec!["Failed: cancelled by operator".to_string()],
        });
        app.recent_qsos_scroll = app.recent_qsos.len().saturating_sub(1);

        term.draw(|f| render_recent_qsos_panel(f, f.area(), &app))
            .unwrap();
        let buf2 = term.backend().buffer().clone();
        assert!(buffer_contains(&buf2, "K1ABC"));
        assert!(buffer_contains(&buf2, "Completed"));
        assert!(buffer_contains(&buf2, "KJ5NJF"));
        assert!(buffer_contains(&buf2, "Failed: watchdog timeout"));
        assert!(buffer_contains(&buf2, "W2XYZ"));
        assert!(buffer_contains(&buf2, "Failed: invalid callsign"));
        assert!(buffer_contains(&buf2, "N3TEST"));
        assert!(buffer_contains(&buf2, "Failed: cancelled by operator"));
        // Header reflects the ring size / scroll position, same convention
        // as the Diagnostics overlay's "(n/total)" title.
        assert!(buffer_contains(&buf2, "Recent QSOs (4/4)"));

        // Also must not panic at tiny/degenerate sizes (mirrors the
        // Station Health smoke test's underflow guard).
        for (w, h) in [(1u16, 1u16), (0, 0), (10, 2), (19, 5)] {
            let backend = TestBackend::new(w.max(1), h.max(1));
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| render_recent_qsos_panel(f, f.area(), &app))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn title_bar_shows_own_station_entity_when_resolvable() {
        let mut config = crate::config::Config::default();
        config.station.call_sign = "K5ARH".to_string();
        let mut app = crate::app::App::new(config, None).await.unwrap();
        app.active_view = crate::view::ActiveView::Operate;
        // 140, not the file's usual 120: at 120 columns this exact scenario's
        // baseline chips (title/call/grid/freq/band/mode/TX-policy/FREQ/
        // DECODE + clock) sum to 121 — one character over the width the
        // entity-omit-if-overflowing logic (see render_title_bar) allows,
        // which would suppress the very thing this test verifies. This test
        // proves the entity CAN render; the omit-when-tight behavior itself
        // is covered by a separate test at the standard 120-column width.
        let backend = TestBackend::new(140, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(
            buffer_contains(&buf, "(United States)"),
            "title bar missing own-station entity"
        );
    }

    /// #171: the title bar has no wrap/overflow handling (a plain unwrapped
    /// `Paragraph`) — an unconditional entity span can silently clip a later
    /// chip once several others are active. At the standard 120-column width,
    /// this exact scenario (K5ARH + a completed-QSO counter chip) has no room
    /// left for the entity, so `render_title_bar` must omit it rather than
    /// clip the QSOs counter that comes after it.
    #[tokio::test]
    async fn title_bar_omits_entity_when_row_has_no_room() {
        let mut config = crate::config::Config::default();
        config.station.call_sign = "K5ARH".to_string();
        let mut app = crate::app::App::new(config, None).await.unwrap();
        app.active_view = crate::view::ActiveView::Operate;
        app.session_completed = 3;
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(
            !buffer_contains(&buf, "(United States)"),
            "entity should be omitted when there's no room for it"
        );
        assert!(
            buffer_contains(&buf, "QSOs: 3"),
            "omitting the entity must free up room for the QSOs counter"
        );
    }
}

/// Task 19: pure geometry tests for `view_rects`/`hit_test` — the shared
/// rect map that makes mouse clicking reliable. No terminal/Frame involved;
/// these test the `Layout::split` math directly, the same math
/// `view_render_tests` above proves is unchanged by rendering through it.
#[cfg(test)]
mod hit_test_tests {
    use super::*;
    use crate::view::ActiveView;

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn assert_no_overlaps(rects: &[(ActivePanel, Rect)]) {
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    !rects_overlap(rects[i].1, rects[j].1),
                    "{:?} ({:?}) overlaps {:?} ({:?})",
                    rects[i].0,
                    rects[i].1,
                    rects[j].0,
                    rects[j].1
                );
            }
        }
    }

    /// Step 1 of the brief: Operate returns 5 non-overlapping rects, all
    /// within the content region, and every rect has positive area (no
    /// degenerate 0-width/0-height panel). The 5 rects don't tile the FULL
    /// content region pixel-for-pixel (the active-QSO banner and the
    /// station card occupy real estate too, but neither is an
    /// `ActivePanel` so `view_rects` never returns them) — this asserts
    /// the union still covers the large majority of the area as a sanity
    /// check on "covering the content region".
    #[test]
    fn view_rects_operate_returns_5_nonoverlapping_rects_covering_content() {
        let area = Rect::new(0, 0, 120, 40);
        let rects = view_rects(ActiveView::Operate, false, ActivePanel::BandActivity, area);
        assert_eq!(rects.len(), 5, "Operate should show exactly 5 panels");

        let mut seen: Vec<ActivePanel> = Vec::new();
        let mut covered_area: u64 = 0;
        for (panel, rect) in &rects {
            assert!(!seen.contains(panel), "duplicate panel {:?}", panel);
            seen.push(*panel);
            assert!(
                rect.width > 0 && rect.height > 0,
                "{:?} is degenerate",
                panel
            );
            assert!(
                rect.x >= area.x
                    && rect.y >= area.y
                    && rect.x + rect.width <= area.x + area.width
                    && rect.y + rect.height <= area.y + area.height,
                "{:?} rect {:?} escapes content area {:?}",
                panel,
                rect,
                area
            );
            covered_area += rect.width as u64 * rect.height as u64;
        }
        assert_no_overlaps(&rects);

        let total_area = area.width as u64 * area.height as u64;
        assert!(
            covered_area * 100 >= total_area * 80,
            "5 panels should cover most of the content region: {covered_area}/{total_area}"
        );
    }

    #[test]
    fn view_rects_monitor_returns_only_band_activity() {
        let area = Rect::new(0, 0, 120, 40);
        let rects = view_rects(ActiveView::Monitor, false, ActivePanel::BandActivity, area);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, ActivePanel::BandActivity);
    }

    #[test]
    fn view_rects_hunt_has_no_callers_and_run_has_no_dx_hunter() {
        let area = Rect::new(0, 0, 120, 40);
        let hunt = view_rects(ActiveView::Hunt, false, ActivePanel::DxHunter, area);
        assert!(hunt.iter().any(|(p, _)| *p == ActivePanel::DxHunter));
        assert!(!hunt.iter().any(|(p, _)| *p == ActivePanel::Callers));
        assert_no_overlaps(&hunt);

        let run = view_rects(ActiveView::Run, false, ActivePanel::Callers, area);
        assert!(run.iter().any(|(p, _)| *p == ActivePanel::Callers));
        assert!(!run.iter().any(|(p, _)| *p == ActivePanel::DxHunter));
        assert_no_overlaps(&run);
    }

    #[test]
    fn view_rects_zoomed_reserves_the_banner_row_above_the_active_panel() {
        let area = Rect::new(0, 0, 120, 40);
        let rects = view_rects(ActiveView::Run, true, ActivePanel::DxHunter, area);
        assert_eq!(
            rects,
            vec![(ActivePanel::DxHunter, Rect::new(0, 1, 120, 39))]
        );
    }

    /// Step 1 of the brief: a point inside Band Activity's data region
    /// (row 3, i.e. 3 rows below its border+header) resolves to
    /// `(BandActivity, 3)`.
    #[test]
    fn hit_test_band_activity_row_3() {
        let area = Rect::new(0, 0, 120, 40);
        let rects = view_rects(ActiveView::Operate, false, ActivePanel::BandActivity, area);
        let (_, band_activity_rect) = rects
            .into_iter()
            .find(|(p, _)| *p == ActivePanel::BandActivity)
            .unwrap();

        // border(1) + header(1) + 3 data rows in.
        let y = band_activity_rect.y + 2 + 3;
        let x = band_activity_rect.x + 2;

        let hit = hit_test(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            area,
            x,
            y,
        );
        assert_eq!(hit, Some((ActivePanel::BandActivity, 3)));
    }

    /// Step 1 of the brief: a point outside every panel rect (e.g. the
    /// status bar, which lives below the content `area` passed in) returns
    /// `None`.
    #[test]
    fn hit_test_status_bar_returns_none() {
        let area = Rect::new(0, 0, 120, 40);
        // Below the content area entirely — where the status bar would be.
        let hit = hit_test(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            area,
            5,
            area.y + area.height + 1,
        );
        assert_eq!(hit, None);
    }

    /// A click on the TxPlacement panel's STRIP row (row 0, right after the
    /// border — no header) resolves to row 0; a click further down (e.g.
    /// the park line, row 3) resolves to that row instead — the
    /// click-to-park handler uses this to tell "strip click" apart from
    /// "just focus the panel".
    #[test]
    fn hit_test_tx_placement_strip_row_vs_other_rows() {
        let area = Rect::new(0, 0, 120, 40);
        let rects = view_rects(ActiveView::Run, false, ActivePanel::Callers, area);
        let (_, tx_rect) = rects
            .into_iter()
            .find(|(p, _)| *p == ActivePanel::TxPlacement)
            .unwrap();

        let strip_hit = hit_test(
            ActiveView::Run,
            false,
            ActivePanel::Callers,
            area,
            tx_rect.x + 2,
            tx_rect.y + 1, // border(1) + row 0 (strip)
        );
        assert_eq!(strip_hit, Some((ActivePanel::TxPlacement, 0)));

        let park_line_hit = hit_test(
            ActiveView::Run,
            false,
            ActivePanel::Callers,
            area,
            tx_rect.x + 2,
            tx_rect.y + 1 + 3, // border(1) + row 3 (park line)
        );
        assert_eq!(park_line_hit, Some((ActivePanel::TxPlacement, 3)));
    }

    /// Zoomed: a click anywhere in the (now full-content-area) rect
    /// resolves to the zoomed panel, row-offset by ITS OWN table/instrument
    /// convention (DxHunter here is a table: border+header offset 2).
    #[test]
    fn hit_test_respects_zoom() {
        let area = Rect::new(0, 0, 120, 40);
        let hit = hit_test(
            ActiveView::Operate,
            true,
            ActivePanel::DxHunter,
            area,
            10,
            area.y + 2 + 5,
        );
        assert_eq!(hit, Some((ActivePanel::DxHunter, 4)));
    }

    /// Issue #96: below the `draw()` resize-guard floor (MIN_TERMINAL_*),
    /// no panels are actually rendered (just a "resize the window" prompt),
    /// so a click must never resolve to a phantom panel rect.
    #[test]
    fn hit_test_returns_none_below_resize_guard_floor() {
        let too_narrow = content_area(Rect::new(0, 0, MIN_TERMINAL_WIDTH - 1, MIN_TERMINAL_HEIGHT));
        let hit = hit_test(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            too_narrow,
            5,
            too_narrow.y + 2,
        );
        assert_eq!(hit, None, "too-narrow content area must never hit a panel");

        let too_short = content_area(Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT - 1));
        let hit = hit_test(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            too_short,
            5,
            too_short.y + 2,
        );
        assert_eq!(hit, None, "too-short content area must never hit a panel");
    }

    /// A terminal exactly AT the resize-guard floor is NOT degraded (`draw()`
    /// only blocks strictly below it) — clicks there must still resolve
    /// normally, proving the guard uses the content area's own floor rather
    /// than naively comparing against the full-terminal MIN_TERMINAL_* consts
    /// (which would reject this valid, non-degraded size too).
    #[test]
    fn hit_test_still_works_exactly_at_resize_guard_floor() {
        let at_floor = content_area(Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT));
        let rects = view_rects(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            at_floor,
        );
        let (_, band_activity_rect) = rects
            .into_iter()
            .find(|(p, _)| *p == ActivePanel::BandActivity)
            .unwrap();

        let hit = hit_test(
            ActiveView::Operate,
            false,
            ActivePanel::BandActivity,
            at_floor,
            band_activity_rect.x + 2,
            band_activity_rect.y + 2,
        );
        assert_eq!(hit, Some((ActivePanel::BandActivity, 0)));
    }
}
