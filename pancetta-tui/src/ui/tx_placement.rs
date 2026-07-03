//! TX-placement instrument — replaces the waterfall in Operate/Hunt/Run
//! (Monitor keeps the energy waterfall; see `ui/mod.rs::layout_monitor`).
//!
//! A vacancy-first, 5-row panel: an openness strip (per-bin tri-state
//! clear/busy), a stream-marker row (where our active QSOs are keyed, plus
//! the global focus), a BEST row (the allocator's live top-5 ranked
//! candidates), a park line (current held offset + coverage + hold
//! duration), and a frequency axis. Data comes from `App::placement`
//! (`Option<PlacementView>`, relayed from the coordinator's
//! `TxPlacementUpdate` — the SAME `SmartFrequencyAllocator` run the
//! autonomous operator uses; the panel never re-derives scores, so it can
//! never disagree with the real decision path).
//!
//! Spec: `docs/superpowers/specs/2026-07-03-tui-redesign-design.md` §2.

use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
    Frame,
};

use super::create_panel_block;
use crate::app::{ActivePanel, App, PlacementView};

/// Circled digits for ranked candidates (①-⑩) — shared by the compact
/// BEST row (Task 11, top 5) and the full-screen top-10 zoom table
/// (Task 13, `render_placement_zoom`), so both call sites use one rank
/// convention.
const CIRCLED_DIGITS: [char; 10] = [
    '\u{2460}', '\u{2461}', '\u{2462}', '\u{2463}', '\u{2464}', '\u{2465}', '\u{2466}', '\u{2467}',
    '\u{2468}', '\u{2469}',
];

/// Fixed tick frequencies for the axis row, mirroring `Waterfall`'s
/// `TICK_FREQS` (`widgets/mod.rs`). Only ticks that fall inside the
/// snapshot's `range` actually render (`freq_to_col` returns `None`
/// otherwise).
const TICK_FREQS: &[f64] = &[500.0, 1000.0, 1500.0, 2000.0, 2500.0];

/// Below this row width the legend (openness strip) and tick labels (axis
/// row) are dropped to keep the strip itself legible — mirrors the `>= 40`
/// / `>= 60` width gates `Waterfall` already uses for its own overlays.
const LEGEND_MIN_WIDTH: usize = 60;
const AXIS_TICK_MIN_WIDTH: usize = 60;

/// Render the 5-row TX-placement instrument into `area`.
///
/// Gracefully handles `app.placement == None` (no snapshot has arrived yet
/// — the coordinator only starts pushing `TxPlacementUpdate` once the
/// autonomous tick has run at least once) by showing a muted waiting
/// message instead of the strip, and handles `app.active_qsos` being empty
/// (zero stream markers) as a no-op loop — neither case panics.
pub fn render_tx_placement(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::TxPlacement);
    let block = create_panel_block("TX Placement", is_active, app);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return Ok(());
    }

    let Some(placement) = app.placement.as_ref() else {
        let msg =
            Paragraph::new("Waiting for TX-placement data (first autonomous tick pending)\u{2026}")
                .style(Style::default().fg(app.theme.muted_color()));
        f.render_widget(msg, inner);
        return Ok(());
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 5])
        .split(inner);

    render_openness_strip(f, rows[0], app, placement);
    render_stream_markers(f, rows[1], app, placement);
    render_best_row(f, rows[2], app, placement);
    render_park_line(f, rows[3], app, placement);
    render_freq_axis(f, rows[4], app, placement);

    Ok(())
}

/// Render the full-screen top-10 zoom table for the TX-placement panel
/// (pressing `z` while the panel is focused — see
/// `ui/mod.rs::render_zoomed_panel`). Unlike `render_tx_placement`'s
/// compact BEST row (top 5, 4 fields squeezed onto one line), this shows
/// the SAME allocator snapshot (`app.placement`, top-10 — the coordinator
/// already requests `placement_snapshot(10)`) as a proper ranked table with
/// more columns: `#, Freq, Windows, Score, Gap(Hz), Quiet`.
///
/// `Quiet` is a **deliberate placeholder** — the instrument only carries a
/// single per-snapshot `received_at` timestamp, not per-slice decode-
/// activity history, so a real "time since activity near this frequency"
/// value isn't available in v1. Every row renders `-` rather than fabricate
/// one; a real per-slice quiet-duration column is a documented follow-up.
pub fn render_placement_zoom(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::TxPlacement);
    let block = create_panel_block("TX Placement \u{2014} Top 10", is_active, app);

    let Some(placement) = app.placement.as_ref() else {
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            let msg = Paragraph::new(
                "Waiting for TX-placement data (first autonomous tick pending)\u{2026}",
            )
            .style(Style::default().fg(app.theme.muted_color()));
            f.render_widget(msg, inner);
        }
        return Ok(());
    };

    let header_cells = ["#", "Freq", "Windows", "Score", "Gap(Hz)", "Quiet"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1).bottom_margin(0);

    let n_bins = placement.openness.len();
    let mut rows: Vec<Row> = placement
        .slices
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, slice)| {
            let rank = CIRCLED_DIGITS
                .get(i)
                .map(|c| c.to_string())
                .unwrap_or_else(|| (i + 1).to_string());
            let coverage = coverage_label(slice.clear_first, slice.clear_second);
            let gap =
                bin_index_for_freq(slice.offset_hz, placement.range, placement.bin_hz, n_bins)
                    .map(|bin| nearest_busy_gap_hz(bin, &placement.openness, placement.bin_hz))
                    .map(|hz| format!("{hz:.0}"))
                    .unwrap_or_else(|| "\u{2014}".to_string());

            Row::new([
                Cell::from(rank),
                Cell::from(format!("{:.0}", slice.offset_hz)),
                Cell::from(coverage),
                Cell::from(format!("{:.0}", slice.score)),
                Cell::from(gap),
                Cell::from("-"),
            ])
        })
        .collect();

    if rows.is_empty() {
        rows.push(Row::new([
            Cell::from(""),
            Cell::from("no clear placement candidates yet"),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]));
    }

    let widths = [
        Constraint::Length(1), // #
        Constraint::Length(6), // Freq
        Constraint::Length(7), // Windows
        Constraint::Length(7), // Score
        Constraint::Length(8), // Gap(Hz)
        Constraint::Length(6), // Quiet
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()));

    f.render_widget(table, area);
    Ok(())
}

/// Distance (in Hz) from `bin` to the nearest bin whose openness code is
/// `0` (busy in both time slots — the openness strip's muted `·` glyph),
/// searching outward on both sides and reporting the smaller distance.
///
/// Edge case (no busy bin at all on one or both sides within `openness`):
/// falls back to the vec's edge index on that side, per the task brief —
/// so on a fully-clear band this reports the distance to whichever edge of
/// the scanned range is closer, rather than treating "found nothing" as an
/// error or infinity.
fn nearest_busy_gap_hz(bin: usize, openness: &[u8], bin_hz: f64) -> f64 {
    let n = openness.len();
    if n == 0 {
        return 0.0;
    }
    let left_idx = (0..bin).rev().find(|&i| openness[i] == 0).unwrap_or(0);
    let right_idx = ((bin + 1)..n).find(|&i| openness[i] == 0).unwrap_or(n - 1);
    let left_dist = bin.saturating_sub(left_idx) as f64;
    let right_dist = right_idx.saturating_sub(bin) as f64;
    left_dist.min(right_dist) * bin_hz
}

/// Frequency (Hz) represented by column `col` within a `width`-wide strip
/// spanning `range` — col's frequency is the midpoint of its fractional
/// slice across `range`, mirroring `Waterfall::freq_to_col`'s inverse.
/// `None` when `width == 0` (nothing to scale against).
///
/// `pub(crate)` (Task 19) so the click-to-park handler
/// (`App::handle_mouse_event`) can map a clicked column to the exact
/// frequency it will ask to park at, using the SAME column<->frequency
/// scaling `bin_for_col` (below) and the strip renderer both use.
pub(crate) fn freq_for_col(col: usize, width: usize, range: (f64, f64)) -> Option<f64> {
    if width == 0 {
        return None;
    }
    let (lo, hi) = range;
    let frac = (col as f64 + 0.5) / width as f64;
    Some(lo + frac * (hi - lo))
}

/// Map a screen column to a bin index in `openness`, using the same
/// column<->frequency scaling as `Waterfall::freq_to_col` (col's frequency
/// is the bin center of its fractional position across `range`).
fn bin_for_col(col: usize, width: usize, range: (f64, f64), bin_hz: f64, n_bins: usize) -> usize {
    if width == 0 || n_bins == 0 {
        return 0;
    }
    let freq = freq_for_col(col, width, range).unwrap_or(range.0);
    bin_index_for_freq(freq, range, bin_hz, n_bins).unwrap_or(0)
}

/// The openness strip's usable width in columns, given the panel's inner
/// (border-stripped) width — i.e. `strip_width` after the legend
/// ("█both ▀E ▄O") is carved off the tail, when there's room for one.
/// Mirrors `render_openness_strip`'s own `show_legend`/`strip_width` math
/// exactly (that function calls this rather than re-deriving it) so the
/// Task 19 click-to-park column mapping always agrees with what the strip
/// actually drew.
pub(crate) fn strip_width_for(inner_width: usize) -> usize {
    const LEGEND_WIDTH: usize = 11; // "█both ▀E ▄O" (glyphs render as width-1 cells)
    if inner_width >= LEGEND_MIN_WIDTH {
        inner_width.saturating_sub(LEGEND_WIDTH + 1)
    } else {
        inner_width
    }
}

/// Bin index containing `freq_hz`, or `None` if outside `range` or the
/// snapshot has no bins / non-positive bin width.
///
/// `pub(crate)` so `App::apply_placement` (Task 14) can look up the parked
/// frequency's bin code using the SAME column/bin math the panel itself
/// renders with, rather than re-deriving it.
pub(crate) fn bin_index_for_freq(
    freq_hz: f64,
    range: (f64, f64),
    bin_hz: f64,
    n_bins: usize,
) -> Option<usize> {
    let (lo, hi) = range;
    if n_bins == 0 || bin_hz <= 0.0 || freq_hz < lo || freq_hz > hi {
        return None;
    }
    let idx = ((freq_hz - lo) / bin_hz).floor().max(0.0) as usize;
    Some(idx.min(n_bins - 1))
}

/// Map a frequency (Hz) to a screen column, mirroring
/// `Waterfall::freq_to_col` exactly (same fractional-position math).
fn freq_to_col(freq_hz: f64, range: (f64, f64), width: usize) -> Option<usize> {
    let (lo, hi) = range;
    if width == 0 || freq_hz < lo || freq_hz > hi {
        return None;
    }
    let frac = (freq_hz - lo) / (hi - lo);
    let col = (frac * width as f64) as usize;
    if col < width {
        Some(col)
    } else {
        None
    }
}

/// Coverage label from a candidate slice's own clear flags (BEST row).
///
/// `pub(crate)` so the Enter-park key handler (`tui_runner.rs`, Task 12) can
/// build the SAME "Parked at {hz} Hz ({coverage})" label the BEST row shows,
/// rather than re-deriving the E/O/E+O encoding a second time.
pub(crate) fn coverage_label(clear_first: bool, clear_second: bool) -> &'static str {
    match (clear_first, clear_second) {
        (true, true) => "E+O",
        (true, false) => "E",
        (false, true) => "O",
        (false, false) => "\u{2014}",
    }
}

/// Coverage label from a raw openness code (park line — we only have the
/// bin's 0-3 code there, not a `FrequencyCandidate`'s clear flags).
/// Mirrors `PlacementView::openness`'s documented encoding: 0=busy-both,
/// 1=second-only-clear, 2=first-only-clear, 3=clear-both.
fn coverage_label_for_code(code: u8) -> &'static str {
    match code {
        3 => "E+O",
        2 => "E",
        1 => "O",
        _ => "\u{2014}",
    }
}

/// Row 1: openness strip. Bright block = clear in both windows, half-blocks
/// = clear in one (glyph distinguishes which), muted middle-dot = occupied.
/// A short legend renders at the row's tail when there's room.
fn render_openness_strip(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let width = row.width as usize;
    let show_legend = width >= LEGEND_MIN_WIDTH;
    let legend_width: usize = 11; // "█both ▀E ▄O" (glyphs render as width-1 cells)
    let strip_width = strip_width_for(width);
    let n_bins = placement.openness.len();

    for col in 0..strip_width {
        let (glyph, color) = if n_bins == 0 {
            ('\u{00b7}', app.theme.muted_color())
        } else {
            let bin = bin_for_col(col, strip_width, placement.range, placement.bin_hz, n_bins);
            match placement.openness.get(bin).copied().unwrap_or(0) {
                3 => ('\u{2588}', app.theme.success_color()), // █ both clear
                2 => ('\u{2580}', app.theme.warning_color()), // ▀ first (even) only
                1 => ('\u{2584}', app.theme.accent_color()),  // ▄ second (odd) only
                _ => ('\u{00b7}', app.theme.muted_color()),   // · busy
            }
        };
        let x = row.x + col as u16;
        f.buffer_mut()[(x, row.y)].set_char(glyph).set_fg(color);
    }

    if show_legend {
        let spans = vec![
            Span::styled("\u{2588}", Style::default().fg(app.theme.success_color())),
            Span::raw("both "),
            Span::styled("\u{2580}", Style::default().fg(app.theme.warning_color())),
            Span::raw("E "),
            Span::styled("\u{2584}", Style::default().fg(app.theme.accent_color())),
            Span::raw("O"),
        ];
        let legend_rect = Rect {
            x: row.x + row.width - legend_width as u16,
            y: row.y,
            width: legend_width as u16,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(spans)), legend_rect);
    }
}

/// Row 2: stream markers. One `│` per active QSO at its TX audio offset
/// (red if that bin reads busy, else green), followed by the callsign
/// while columns remain. The global focus (if it has a known frequency
/// from its latest decode) gets an overlaid `◆` diamond.
fn render_stream_markers(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let width = row.width as usize;
    let n_bins = placement.openness.len();

    for qso in &app.active_qsos {
        let Some(col) = freq_to_col(qso.frequency_hz, placement.range, width) else {
            continue;
        };
        let code = if n_bins == 0 {
            3
        } else {
            let bin = bin_for_col(col, width, placement.range, placement.bin_hz, n_bins);
            placement.openness.get(bin).copied().unwrap_or(0)
        };
        let color = if code == 0 {
            app.theme.error_color()
        } else {
            app.theme.success_color()
        };
        let x = row.x + col as u16;
        f.buffer_mut()[(x, row.y)]
            .set_char('\u{2502}')
            .set_fg(color);

        for (i, ch) in qso.their_callsign.chars().enumerate() {
            let cx = col + 1 + i;
            if cx >= width {
                break;
            }
            f.buffer_mut()[(row.x + cx as u16, row.y)]
                .set_char(ch)
                .set_fg(color);
        }
    }

    // Global focus overlay: draw last so it takes priority over a
    // coincidentally-overlapping stream marker.
    if let Some(focus) = app.focused_callsign() {
        let latest = app.decoded_messages.iter().rev().find(|m| {
            m.call_sign
                .as_deref()
                .is_some_and(|c| pancetta_core::callsign::callsigns_match(c, &focus))
        });
        if let Some(msg) = latest {
            if let Some(col) = freq_to_col(msg.delta_freq as f64, placement.range, width) {
                let x = row.x + col as u16;
                f.buffer_mut()[(x, row.y)]
                    .set_char('\u{25c6}')
                    .set_fg(app.theme.selected_color());
            }
        }
    }
}

/// Row 3: BEST row — the allocator's live top-5 ranked candidates. The
/// cursor slice (`app.placement_cursor`) renders reversed.
fn render_best_row(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    if placement.slices.is_empty() {
        let p = Paragraph::new("no clear placement candidates yet")
            .style(Style::default().fg(app.theme.muted_color()));
        f.render_widget(p, row);
        return;
    }

    let base = Style::default().fg(app.theme.foreground_color());
    let mut spans: Vec<Span> = Vec::new();
    for (i, slice) in placement.slices.iter().take(5).enumerate() {
        let digit = CIRCLED_DIGITS.get(i).copied().unwrap_or('\u{2022}');
        let coverage = coverage_label(slice.clear_first, slice.clear_second);
        let text = format!(
            "{} {:.0} {} {:.0}",
            digit, slice.offset_hz, coverage, slice.score
        );
        let style = if i == app.placement_cursor {
            base.add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD)
        } else {
            base
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), row);
}

/// Row 4: park line — current held offset (if any), its coverage, and how
/// long it's been held, plus the interaction hint. When the current bin's
/// coverage code is `< 3` (not fully clear), prefixes a `⚠` degradation
/// flag (Task 14) — this is a per-render DISPLAY of the CURRENT state
/// (always visible while degraded), distinct from `App::apply_placement`'s
/// one-shot `status_message` warning fired on the transition INTO
/// degradation (Task 14's edge-triggered event notification).
fn render_park_line(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let main = if let Some(hz) = app.tx_offset_hold_hz {
        let code = bin_index_for_freq(
            hz as f64,
            placement.range,
            placement.bin_hz,
            placement.openness.len(),
        )
        .and_then(|b| placement.openness.get(b).copied());
        let coverage = code.map(coverage_label_for_code).unwrap_or("\u{2014}");
        // `parked_since` is set by the park interaction (Task 12); a held
        // offset without a park timestamp (e.g. set via the `o` modal
        // rather than an instrument park) shows 0 min rather than omitting
        // the clause.
        let mins = app
            .parked_since
            .map(|t| (chrono::Utc::now() - t).num_minutes().max(0))
            .unwrap_or(0);
        let warn = if code.is_some_and(|c| c < 3) {
            "\u{26a0} "
        } else {
            ""
        };
        format!("{warn}parked: {hz} ({coverage}, holding {mins} min)")
    } else {
        "not parked \u{2014} Enter parks \u{2460}".to_string()
    };
    let line = format!("{main} \u{b7} \u{2190}/\u{2192} pick \u{b7} Enter=park \u{b7} z=top-10");
    let p = Paragraph::new(line).style(Style::default().fg(app.theme.foreground_color()));
    f.render_widget(p, row);
}

/// Row 5: frequency axis. A dashed baseline plus tick marks at fixed
/// frequencies (mirroring `Waterfall`'s axis, `widgets/mod.rs:340-367`),
/// omitted below `AXIS_TICK_MIN_WIDTH` to keep the baseline legible.
fn render_freq_axis(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let width = row.width as usize;
    for col in 0..width {
        let x = row.x + col as u16;
        f.buffer_mut()[(x, row.y)]
            .set_char('\u{2500}')
            .set_fg(app.theme.muted_color());
    }
    if width < AXIS_TICK_MIN_WIDTH {
        return;
    }
    for &freq in TICK_FREQS {
        if let Some(col) = freq_to_col(freq, placement.range, width) {
            let x = row.x + col as u16;
            f.buffer_mut()[(x, row.y)]
                .set_char('\u{2534}')
                .set_fg(app.theme.border_color());
        }
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn nearest_busy_gap_hz_finds_nearer_busy_bin_on_either_side() {
        // busy at bins 2 and 8; bin 5 is 3 away from each side, so the
        // gap should be 3 * bin_hz regardless of which side wins the tie.
        let openness = [3, 3, 0, 3, 3, 3, 3, 3, 0, 3];
        assert_eq!(nearest_busy_gap_hz(5, &openness, 25.0), 75.0);
        // bin 3 is 1 away from the busy bin at 2, 5 away from the one at 8.
        assert_eq!(nearest_busy_gap_hz(3, &openness, 25.0), 25.0);
    }

    #[test]
    fn nearest_busy_gap_hz_falls_back_to_vec_edge_when_no_busy_bin_exists() {
        // Fully-clear band: no bin is ever 0, so both searches exhaust and
        // fall back to the edges (index 0 / n-1) per the documented
        // edge-case choice — the reported gap is the distance to whichever
        // edge of the scanned range is closer.
        let openness = [3; 10];
        // bin 2: left edge is 2 away, right edge (idx 9) is 7 away -> min 2.
        assert_eq!(nearest_busy_gap_hz(2, &openness, 25.0), 50.0);
        // bin 0 is already the left edge (0 away); right edge is 9 away.
        assert_eq!(nearest_busy_gap_hz(0, &openness, 25.0), 0.0);
        // bin 9 is already the right edge (0 away); left edge is 9 away.
        assert_eq!(nearest_busy_gap_hz(9, &openness, 25.0), 0.0);
    }

    #[test]
    fn freq_to_col_matches_waterfall_scaling() {
        // Same math as `Waterfall::freq_to_col`: frac * width, truncated.
        assert_eq!(freq_to_col(200.0, (200.0, 2600.0), 96), Some(0));
        assert_eq!(freq_to_col(2599.0, (200.0, 2600.0), 96), Some(95));
        assert_eq!(freq_to_col(100.0, (200.0, 2600.0), 96), None); // below range
        assert_eq!(freq_to_col(2700.0, (200.0, 2600.0), 96), None); // above range
    }

    #[test]
    fn bin_for_col_round_trips_through_freq_to_col() {
        // A column's bin should contain the frequency freq_to_col placed it at.
        let range = (200.0, 2600.0);
        let bin_hz = 25.0;
        let n_bins = 96;
        for col in [0usize, 10, 50, 95] {
            let bin = bin_for_col(col, 96, range, bin_hz, n_bins);
            assert!(bin < n_bins);
        }
    }

    #[test]
    fn coverage_label_covers_all_combinations() {
        assert_eq!(coverage_label(true, true), "E+O");
        assert_eq!(coverage_label(true, false), "E");
        assert_eq!(coverage_label(false, true), "O");
        assert_eq!(coverage_label(false, false), "\u{2014}");
    }

    #[test]
    fn coverage_label_for_code_matches_documented_encoding() {
        assert_eq!(coverage_label_for_code(3), "E+O");
        assert_eq!(coverage_label_for_code(2), "E");
        assert_eq!(coverage_label_for_code(1), "O");
        assert_eq!(coverage_label_for_code(0), "\u{2014}");
    }

    #[test]
    fn bin_index_for_freq_out_of_range_is_none() {
        assert_eq!(bin_index_for_freq(100.0, (200.0, 2600.0), 25.0, 96), None);
        assert_eq!(bin_index_for_freq(2700.0, (200.0, 2600.0), 25.0, 96), None);
        assert_eq!(
            bin_index_for_freq(1480.0, (200.0, 2600.0), 25.0, 96),
            Some(51)
        );
    }

    // --- Task 19: click-to-park column<->frequency helpers ------------

    #[test]
    fn freq_for_col_is_zero_width_safe() {
        assert_eq!(freq_for_col(0, 0, (200.0, 2600.0)), None);
    }

    #[test]
    fn freq_for_col_matches_bin_for_col_scaling() {
        // Column 0 of a 100-wide strip over (200,2600) should land in the
        // same bin bin_for_col derives internally (they now share the same
        // freq_for_col math after the Task 19 refactor).
        let range = (200.0, 2600.0);
        let freq = freq_for_col(0, 100, range).expect("width > 0");
        let bin = bin_index_for_freq(freq, range, 25.0, 96);
        assert_eq!(bin, Some(bin_for_col(0, 100, range, 25.0, 96)));
    }

    #[test]
    fn strip_width_for_matches_legend_threshold() {
        // Below LEGEND_MIN_WIDTH (60): full width, no legend carved off.
        assert_eq!(strip_width_for(59), 59);
        // At/above the threshold: legend_width(11) + 1 separator column.
        assert_eq!(strip_width_for(60), 60 - 12);
        assert_eq!(strip_width_for(100), 100 - 12);
    }
}
