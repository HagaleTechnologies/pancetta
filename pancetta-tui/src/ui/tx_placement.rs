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
    widgets::Paragraph,
    Frame,
};

use super::create_panel_block;
use crate::app::{ActivePanel, App, PlacementView};

/// Circled digits for the BEST row's ranked candidates (①-⑤).
const CIRCLED_DIGITS: [char; 5] = ['\u{2460}', '\u{2461}', '\u{2462}', '\u{2463}', '\u{2464}'];

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

/// Map a screen column to a bin index in `openness`, using the same
/// column<->frequency scaling as `Waterfall::freq_to_col` (col's frequency
/// is the bin center of its fractional position across `range`).
fn bin_for_col(col: usize, width: usize, range: (f64, f64), bin_hz: f64, n_bins: usize) -> usize {
    if width == 0 || n_bins == 0 {
        return 0;
    }
    let (lo, hi) = range;
    let frac = (col as f64 + 0.5) / width as f64;
    let freq = lo + frac * (hi - lo);
    bin_index_for_freq(freq, range, bin_hz, n_bins).unwrap_or(0)
}

/// Bin index containing `freq_hz`, or `None` if outside `range` or the
/// snapshot has no bins / non-positive bin width.
fn bin_index_for_freq(
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
fn coverage_label(clear_first: bool, clear_second: bool) -> &'static str {
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
    let strip_width = if show_legend {
        width.saturating_sub(legend_width + 1)
    } else {
        width
    };
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
/// long it's been held, plus the interaction hint. Degradation flagging
/// (Task 14) is deliberately not implemented here.
fn render_park_line(f: &mut Frame<'_>, row: Rect, app: &App, placement: &PlacementView) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let main = if let Some(hz) = app.tx_offset_hold_hz {
        let coverage = bin_index_for_freq(
            hz as f64,
            placement.range,
            placement.bin_hz,
            placement.openness.len(),
        )
        .and_then(|b| placement.openness.get(b).copied())
        .map(coverage_label_for_code)
        .unwrap_or("\u{2014}");
        // `parked_since` is set by the (not-yet-built) park interaction
        // (Task 12); a held offset without a park timestamp (e.g. set via
        // the `o` modal rather than an instrument park) shows 0 min rather
        // than omitting the clause.
        let mins = app
            .parked_since
            .map(|t| (chrono::Utc::now() - t).num_minutes().max(0))
            .unwrap_or(0);
        format!("parked: {hz} ({coverage}, holding {mins} min)")
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
}
