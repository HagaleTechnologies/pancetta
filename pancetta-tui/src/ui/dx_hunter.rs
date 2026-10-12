use anyhow::Result;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Cell, Row, Table, TableState},
    Frame,
};

use super::{create_panel_block, format_bearing, format_distance, format_time_ago};
use crate::app::{ActivePanel, App, DxStation, SpotSource};

/// Format the DX Hunter SNR cell. A pure NETWORK spot that carried no SNR
/// stores `snr: 0`, which is indistinguishable from a real 0 dB — render that
/// case as "---" so a missing value isn't read as a strong-but-zero signal.
/// Local (and Both) spots always carry a measured SNR and render with a sign.
fn format_dx_snr(source: &SpotSource, snr: i32, best_snr_network: Option<i32>) -> String {
    if *source == SpotSource::Network && best_snr_network.is_none() {
        "---".to_string()
    } else {
        format!("{snr:+}")
    }
}

/// Row-highlight marker for the selected station. Also part of the column
/// budget in [`visible_columns`] (PAN-88, D4).
const HIGHLIGHT_SYMBOL: &str = "▶ ";

/// Single source of truth for DX Hunter's column set (PAN-88), mirroring
/// Band Activity's `BandActivityColumn`: the header, the widths and every
/// row's cells derive from [`visible_columns`], so they can't drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DxHunterColumn {
    Src,
    Call,
    Entity,
    Grid,
    Snr,
    Rarity,
    Rpt,
    Last,
    Pri,
}

impl DxHunterColumn {
    /// Display order (left to right).
    pub(super) const ALL: [DxHunterColumn; 9] = [
        DxHunterColumn::Src,
        DxHunterColumn::Call,
        DxHunterColumn::Entity,
        DxHunterColumn::Grid,
        DxHunterColumn::Snr,
        DxHunterColumn::Rarity,
        DxHunterColumn::Rpt,
        DxHunterColumn::Last,
        DxHunterColumn::Pri,
    ];

    /// The order columns join as the panel widens: the call (with its need
    /// markers), what entity it is, how strong it is and how much pancetta
    /// wants it come first; `Src`/`Rpt` are diagnostics. Freshness is
    /// already shown by dimming stale rows, so `Last` can wait for room.
    const PRIORITY: [DxHunterColumn; 9] = [
        DxHunterColumn::Call,
        DxHunterColumn::Entity,
        DxHunterColumn::Snr,
        DxHunterColumn::Pri,
        DxHunterColumn::Last,
        DxHunterColumn::Rarity,
        DxHunterColumn::Grid,
        DxHunterColumn::Rpt,
        DxHunterColumn::Src,
    ];

    pub(super) fn header(self) -> &'static str {
        match self {
            DxHunterColumn::Src => "Src",
            DxHunterColumn::Call => "Call",
            DxHunterColumn::Entity => "Entity",
            DxHunterColumn::Grid => "Grid",
            DxHunterColumn::Snr => "SNR",
            DxHunterColumn::Rarity => "Rarity",
            DxHunterColumn::Rpt => "Rpt",
            DxHunterColumn::Last => "Last",
            DxHunterColumn::Pri => "Pri",
        }
    }

    fn width(self) -> Constraint {
        match self {
            DxHunterColumn::Src => Constraint::Length(4),
            // !/+ need + ★ markers + up to ~8 chars.
            DxHunterColumn::Call => Constraint::Length(12),
            // Flexes into the freed Freq space.
            DxHunterColumn::Entity => Constraint::Min(8),
            DxHunterColumn::Grid => Constraint::Length(4),
            DxHunterColumn::Snr => Constraint::Length(4),
            DxHunterColumn::Rarity => Constraint::Length(7),
            DxHunterColumn::Rpt => Constraint::Length(3),
            DxHunterColumn::Last => Constraint::Length(5),
            DxHunterColumn::Pri => Constraint::Length(4),
        }
    }

    /// The narrowest this column is ever drawn (its `Length`/`Min`).
    fn min_width(self) -> u16 {
        match self.width() {
            Constraint::Length(n) | Constraint::Min(n) => n,
            _ => 0,
        }
    }
}

/// Columns shown in a DX Hunter panel `width` columns wide (PAN-88).
///
/// D3, prefix rule: walk [`DxHunterColumn::PRIORITY`], adding each column's
/// minimum width plus 1 of spacing, and stop at the first that doesn't fit —
/// monotonic, so widening never removes a column, and no heading is ever
/// squeezed to a stub. D4: the budget always reserves the highlight marker's
/// width (besides the 2 borders) so the set doesn't jump when the operator
/// Tabs into the panel. `Call` is always shown. Returned in display order.
pub(super) fn visible_columns(width: u16) -> Vec<DxHunterColumn> {
    let budget = width
        .saturating_sub(2)
        .saturating_sub(Span::raw(HIGHLIGHT_SYMBOL).width() as u16);
    let n = super::bar_fit::prefix_fit(
        DxHunterColumn::PRIORITY.iter().map(|c| c.min_width()),
        budget,
    )
    .max(1);
    let shown = &DxHunterColumn::PRIORITY[..n];
    DxHunterColumn::ALL
        .into_iter()
        .filter(|c| shown.contains(c))
        .collect()
}

pub fn render_dx_hunter(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::DxHunter);
    let block = create_panel_block("DX Hunter", is_active, app);

    // Prepare table data
    // "Freq" dropped — every DX Hunter spot is on the current band's FT8 dial,
    // so the column was always identical. Replaced with the DXCC "Entity" the
    // operator actually wants to scan. PAN-88: narrow panels show fewer,
    // whole columns (see `visible_columns`).
    let columns = visible_columns(area.width);
    let header_cells = columns.iter().map(|c| {
        Cell::from(c.header()).style(
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells).height(1).bottom_margin(0);

    // Single source of truth for ordering — the SAME list (and comparator)
    // that `App::get_selected_station` indexes with `dx_hunter_scroll`, so
    // the highlighted row is always the Space call-target. Do NOT re-sort
    // or `.skip()` here: let `TableState` own the viewport offset from the
    // selected index, so the cursor and the chooser can never disagree.
    let dx_list: Vec<&DxStation> = app.displayed_dx_stations();

    // Convert ALL stations to rows; TableState scrolls the viewport to keep
    // the selected row visible.
    let mut rows: Vec<Row> = dx_list
        .iter()
        .map(|station| create_dx_row(station, app, &columns))
        .collect();

    // If no DX stations, show placeholder
    if rows.is_empty() {
        rows.push(Row::new(columns.iter().map(|c| {
            let text = match c {
                DxHunterColumn::Call => "No DX",
                DxHunterColumn::Entity => "stations",
                _ => "",
            };
            Cell::from(text).style(Style::default().fg(app.theme.muted_color()))
        })));
    }

    let widths: Vec<Constraint> = columns.iter().map(|c| c.width()).collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()))
        // Visible cursor: reversed video + a leading marker on the
        // selected row so the operator can see exactly which station
        // Space will call.
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(HIGHLIGHT_SYMBOL);

    // The selected index is `dx_hunter_scroll` into the displayed list.
    // `TableState` clamps the viewport so the selection stays on screen,
    // so we no longer hand-roll a skip offset.
    let mut table_state = TableState::default();
    if is_active && !dx_list.is_empty() {
        let sel = app.dx_hunter_scroll.min(dx_list.len().saturating_sub(1));
        table_state.select(Some(sel));
    }

    f.render_stateful_widget(table, area, &mut table_state);

    // Show scroll indicator if there are more stations
    if dx_list.len() > (area.height as usize).saturating_sub(4) {
        let scroll_info = format!("{}/{}", app.dx_hunter_scroll + 1, dx_list.len());

        let scroll_area = Rect {
            x: area.x + area.width.saturating_sub(scroll_info.len() as u16 + 2),
            y: area.y,
            width: scroll_info.len() as u16 + 1,
            height: 1,
        };

        let scroll_text = ratatui::text::Line::from(Span::styled(
            scroll_info,
            Style::default().fg(app.theme.muted_color()),
        ));

        let scroll_widget = ratatui::widgets::Paragraph::new(scroll_text);
        f.render_widget(scroll_widget, scroll_area);
    }

    Ok(())
}

/// The Entity cell: cqdx's authoritative name first, offline prefix
/// resolver second, `"---"` when neither resolves — with the station's
/// US state/territory appended when known (PAN-85).
fn entity_cell_text(station: &DxStation, state: Option<&str>) -> String {
    match station
        .entity_name
        .clone()
        .or_else(|| crate::dxcc::entity_for_callsign(&station.call_sign).map(str::to_string))
    {
        Some(entity) => crate::dxcc::format_entity_with_state(&entity, state),
        None => "---".to_string(),
    }
}

fn create_dx_row<'a>(station: &'a DxStation, app: &App, columns: &[DxHunterColumn]) -> Row<'a> {
    // Cross-panel global focus (Task 2): when some OTHER panel is active and
    // this row's callsign is the operator's current focus, flag it here too
    // — but only when DX Hunter itself is NOT the active panel, so we never
    // double up with the active panel's REVERSED row-highlight.
    let is_active_panel = matches!(app.active_panel, ActivePanel::DxHunter);
    let is_globally_focused = !is_active_panel && app.is_focused(&station.call_sign);

    // Tier-2 highlight (Task 3): one of our current active-QSO partners.
    let is_engaged = app.is_engaged(&station.call_sign);

    // Source indicator
    let src_str = station.source.to_string();
    let src_style = match station.source {
        SpotSource::Local => Style::default().fg(app.theme.success_color()),
        SpotSource::Network => Style::default().fg(app.theme.accent_color()),
        SpotSource::Both => Style::default().fg(ratatui::style::Color::Cyan),
    };

    // Callsign with need/notable markers. `!` = ATNO (all-time new one),
    // `+` = needed DXCC (not ATNO), `★` = notable. Markers stack so an
    // ATNO that's also notable reads "!★CALL". `▲` = band_needed (never
    // worked this entity on THIS band before, per the local QSO database —
    // a signal independent of `atno`/`needed`, which reflect cqdx's needed
    // set for the operator's currently-tuned band, not necessarily this
    // row's own band). Suppressed when `atno` is already set, since ATNO
    // (never worked on ANY band) already implies band-needed and a second
    // marker would be redundant noise. 2026-07-18, DX Hunter
    // per-band-needed gap.
    let call_display = format_call_display(station, is_engaged);

    // Staleness check for network-only spots
    let is_stale = if station.source != SpotSource::Local {
        station
            .last_seen_network
            .map(|ts| {
                let age = chrono::Utc::now().timestamp() - ts;
                age > 600 // >10 minutes
            })
            .unwrap_or(false)
    } else {
        false
    };

    let call_style = if is_stale {
        Style::default().fg(app.theme.muted_color())
    } else if is_globally_focused {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else if station.atno {
        // ATNO — the highest-value catch. Bold magenta + underline so it
        // stands apart from merely-notable (bold magenta) rows.
        Style::default()
            .fg(ratatui::style::Color::Magenta)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else if station.is_notable {
        Style::default()
            .fg(ratatui::style::Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if station.worked_before {
        Style::default().fg(app.theme.muted_color())
    } else if station.needed {
        // Needed DXCC (band-fill, not ATNO): bold accent.
        Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD)
    } else if is_rare_dx_from_tier(station) {
        Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD)
    } else if station.distance.unwrap_or(0.0) > 5000.0 {
        Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    };
    // Tier-2 (engaged) highlight stacks on top of whatever the above chose —
    // green + underline, without discarding e.g. the tier-1 BOLD.
    let call_style = if is_engaged {
        call_style
            .fg(app.theme.success_color())
            .add_modifier(Modifier::UNDERLINED)
    } else {
        call_style
    };

    let grid_str = station.grid_square.as_deref().unwrap_or("---").to_string();
    let entity_str = entity_cell_text(station, app.station_state_for(&station.call_sign));
    // A pure network spot with no reported SNR stores snr:0, which is
    // indistinguishable from a real 0 dB. Render it as "---" so the operator
    // doesn't read a missing value as a strong-but-zero signal. Local decodes
    // (and Both) always carry a real measured SNR.
    let snr_str = format_dx_snr(&station.source, station.snr, station.best_snr_network);
    let rarity_str = station.rarity_tier.as_deref().unwrap_or("-").to_string();
    let rpt_str = station
        .reporter_count
        .map(|r| r.to_string())
        .unwrap_or_default();
    let last_str = format_time_ago(station.last_seen);
    let pri_str = format!("{}", station.priority_score);

    let dim = if is_stale {
        Style::default().fg(app.theme.muted_color())
    } else {
        Style::default().fg(app.theme.foreground_color())
    };

    let rarity_style = match station.rarity_tier.as_deref() {
        Some("legendary") => Style::default()
            .fg(ratatui::style::Color::Magenta)
            .add_modifier(Modifier::BOLD),
        Some("very_rare") => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        Some("rare") => Style::default().fg(app.theme.warning_color()),
        _ => dim,
    };

    let snr_style = Style::default().fg(get_snr_color(station.snr, &app.theme));

    let priority_style = match station.priority_score {
        score
            if score
                >= pancetta_qso::priority::PriorityTier::SpecialStation as u32
                    * pancetta_qso::priority::TIER_BAND_WIDTH =>
        {
            Style::default()
                .fg(app.theme.error_color())
                .add_modifier(Modifier::BOLD)
        }
        score
            if score
                >= pancetta_qso::priority::PriorityTier::PerBandGridNew as u32
                    * pancetta_qso::priority::TIER_BAND_WIDTH =>
        {
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD)
        }
        _ => dim,
    };

    // One cell per visible column (PAN-88), mirroring
    // `band_activity::create_message_row`.
    Row::new(
        [
            (DxHunterColumn::Src, Cell::from(src_str).style(src_style)),
            (
                DxHunterColumn::Call,
                Cell::from(call_display).style(call_style),
            ),
            (
                DxHunterColumn::Entity,
                Cell::from(entity_str).style(Style::default().fg(app.theme.accent_color())),
            ),
            (DxHunterColumn::Grid, Cell::from(grid_str).style(dim)),
            (DxHunterColumn::Snr, Cell::from(snr_str).style(snr_style)),
            (
                DxHunterColumn::Rarity,
                Cell::from(rarity_str).style(rarity_style),
            ),
            (DxHunterColumn::Rpt, Cell::from(rpt_str).style(dim)),
            (DxHunterColumn::Last, Cell::from(last_str).style(dim)),
            (
                DxHunterColumn::Pri,
                Cell::from(pri_str).style(priority_style),
            ),
        ]
        .into_iter()
        .filter(|(col, _)| columns.contains(col))
        .map(|(_, cell)| cell),
    )
}

/// Build the full callsign cell display: need/notable markers, band-needed
/// triangle, DX-watchlist diamond (#197), engaged dot, then the callsign.
/// Factored out of `create_dx_row` so the glyph logic is unit-testable
/// without a full ratatui `Row`.
fn format_call_display(station: &DxStation, is_engaged: bool) -> String {
    format!(
        "{}{}{}{}{}",
        need_marker(station.atno, station.needed, station.is_notable),
        if station.band_needed && !station.atno {
            "▲"
        } else {
            ""
        },
        if station.watchlisted { "◇" } else { "" },
        if is_engaged { "● " } else { "" },
        station.call_sign
    )
}

/// Build the callsign-prefix marker string for the DX Hunter:
/// `!` for ATNO, `+` for a needed (non-ATNO) DXCC, then `★` for notable.
/// ATNO and needed are mutually exclusive (ATNO wins). Returns `""` for a
/// plain station.
fn need_marker(atno: bool, needed: bool, notable: bool) -> String {
    let mut s = String::new();
    if atno {
        s.push('!');
    } else if needed {
        s.push('+');
    }
    if notable {
        s.push('★');
    }
    s
}

fn get_snr_color(snr: i32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match snr {
        snr if snr >= 0 => theme.success_color(),
        snr if snr >= -10 => theme.warning_color(),
        _ => theme.error_color(),
    }
}

/// Check if a station is rare DX using cqdx.io rarity tier (preferred)
/// or fallback hardcoded prefix list.
fn is_rare_dx_from_tier(station: &DxStation) -> bool {
    match station.rarity_tier.as_deref() {
        Some("legendary") | Some("very_rare") => true,
        Some(_) => false,
        None => is_rare_dx_fallback(&station.call_sign),
    }
}

/// Fallback rare DX check when cqdx.io data is unavailable.
fn is_rare_dx_fallback(call_sign: &str) -> bool {
    let rare_prefixes = [
        "1A", "3Y", "4U", "7O", "8Q", "9Q", "BS7", "BV9", "BY9", "CY0", "CY9", "E3", "E4", "EK0",
        "FT/G", "FT/J", "FT/W", "FT/X", "FT/Z", "H40", "HK0", "P5", "S0", "T31", "T32", "T33",
        "VK0H", "VK0M", "VK9C", "VK9L", "VK9M", "VK9N", "VK9W", "VK9X", "VP8G", "VP8H", "VP8O",
        "VP8S", "XF4", "XU", "XW", "XX9", "YJ0", "Z2", "ZS8",
    ];
    rare_prefixes
        .iter()
        .any(|&prefix| call_sign.starts_with(prefix))
}

/// Check if a callsign is from a new DXCC entity
pub fn is_new_dxcc(call_sign: &str, worked_dxcc: &std::collections::HashSet<String>) -> bool {
    let dxcc_prefix = extract_dxcc_prefix(call_sign);
    !worked_dxcc.contains(&dxcc_prefix)
}

/// Extract DXCC prefix from callsign
fn extract_dxcc_prefix(call_sign: &str) -> String {
    // Very basic DXCC prefix extraction
    // In a real implementation, you'd use a proper DXCC database

    let call_upper = call_sign.to_uppercase();

    // Handle special cases with slashes
    if call_upper.contains('/') {
        let parts: Vec<&str> = call_upper.split('/').collect();

        // Portable operations: take the base call
        if parts.len() == 2 {
            if parts[1].len() <= 3 && parts[1].chars().all(|c| c.is_ascii_alphanumeric()) {
                // Likely a portable suffix like /P, /M, /MM
                return extract_base_prefix(parts[0]);
            } else if parts[0].len() <= 3 && parts[0].chars().all(|c| c.is_ascii_alphanumeric()) {
                // Likely a prefix like VK9/
                return parts[0].to_string();
            }
        }
    }

    extract_base_prefix(&call_upper)
}

fn extract_base_prefix(call_sign: &str) -> String {
    // Find the first digit
    if let Some(digit_pos) = call_sign.chars().position(|c| c.is_ascii_digit()) {
        if digit_pos == 0 {
            return call_sign.chars().take(2).collect();
        }

        // Standard format: prefix + digit + suffix
        let prefix_chars: String = call_sign.chars().take(digit_pos + 1).collect();
        return prefix_chars;
    }

    // Fallback: take first 2-3 characters
    call_sign.chars().take(3).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---- PAN-88: adaptive columns ----
    // Canonical panel widths: Operate's 40% right column at 80/100/120/132
    // columns, then the wide tier (160 columns, and Hunt view at 80).

    #[test]
    fn visible_columns_compact_tier_at_operate_80() {
        use DxHunterColumn::*;
        assert_eq!(visible_columns(32), vec![Call, Entity, Snr]);
    }

    #[test]
    fn visible_columns_at_operate_100() {
        use DxHunterColumn::*;
        assert_eq!(visible_columns(40), vec![Call, Entity, Snr, Pri]);
    }

    #[test]
    fn visible_columns_at_operate_120() {
        use DxHunterColumn::*;
        assert_eq!(visible_columns(48), vec![Call, Entity, Snr, Last, Pri]);
    }

    #[test]
    fn visible_columns_at_operate_132() {
        use DxHunterColumn::*;
        assert_eq!(
            visible_columns(53),
            vec![Call, Entity, Snr, Rarity, Last, Pri]
        );
    }

    #[test]
    fn visible_columns_wide_tier_shows_all() {
        assert_eq!(visible_columns(64), DxHunterColumn::ALL.to_vec());
        assert_eq!(visible_columns(80), DxHunterColumn::ALL.to_vec());
    }

    #[test]
    fn visible_columns_are_in_display_order_and_monotonic() {
        for w in 16..=200u16 {
            let cols = visible_columns(w);
            let positions: Vec<usize> = cols
                .iter()
                .map(|c| DxHunterColumn::ALL.iter().position(|a| a == c).unwrap())
                .collect();
            assert!(
                positions.windows(2).all(|p| p[0] < p[1]),
                "width {w}: {cols:?} not in display order"
            );
            let wider = visible_columns(w + 1);
            assert!(
                cols.iter().all(|c| wider.contains(c)),
                "width {w}: {cols:?} not a subset of {wider:?}"
            );
        }
    }

    #[test]
    fn visible_columns_always_fit_their_budget() {
        for w in 16..=200u16 {
            let cols = visible_columns(w);
            let used: u16 = cols.iter().map(|c| c.min_width()).sum::<u16>() + cols.len() as u16 - 1;
            assert!(used <= w - 4, "width {w}: {cols:?} needs {used}");
        }
    }

    #[test]
    fn every_header_is_at_least_as_narrow_as_its_column() {
        for c in DxHunterColumn::ALL {
            assert!(c.header().len() as u16 <= c.min_width(), "{c:?}");
        }
    }

    #[test]
    fn priority_style_thresholds_track_tier_band_width() {
        assert_eq!(
            pancetta_qso::priority::PriorityTier::SpecialStation as u32
                * pancetta_qso::priority::TIER_BAND_WIDTH,
            3000
        );
        assert_eq!(
            pancetta_qso::priority::PriorityTier::PerBandGridNew as u32
                * pancetta_qso::priority::TIER_BAND_WIDTH,
            2000
        );
    }

    #[test]
    fn test_is_rare_dx_fallback() {
        assert!(is_rare_dx_fallback("3Y0X"));
        assert!(is_rare_dx_fallback("VP8STI"));
        assert!(!is_rare_dx_fallback("W1ABC"));
        assert!(!is_rare_dx_fallback("G0XYZ"));
    }

    #[test]
    fn test_extract_dxcc_prefix() {
        assert_eq!(extract_dxcc_prefix("W1ABC"), "W1");
        assert_eq!(extract_dxcc_prefix("G0XYZ"), "G0");
        assert_eq!(extract_dxcc_prefix("JA1ABC"), "JA1");
        assert_eq!(extract_dxcc_prefix("VK9/W1ABC"), "VK9");
        assert_eq!(extract_dxcc_prefix("W1ABC/P"), "W1");
    }

    #[test]
    fn test_is_new_dxcc() {
        let mut worked = HashSet::new();
        worked.insert("W1".to_string());
        worked.insert("G0".to_string());

        assert!(is_new_dxcc("JA1ABC", &worked));
        assert!(!is_new_dxcc("W1XYZ", &worked));
    }

    #[test]
    fn network_spot_without_snr_renders_dashes() {
        // Pure network spot, no reported SNR -> "---" (not "+0").
        assert_eq!(
            format_dx_snr(&SpotSource::Network, 0, None),
            "---",
            "network-no-SNR must show ---"
        );
        // Network spot WITH a real SNR renders it.
        assert_eq!(format_dx_snr(&SpotSource::Network, -7, Some(-7)), "-7");
        // A local 0 dB decode is a real measurement -> "+0", never "---".
        assert_eq!(format_dx_snr(&SpotSource::Local, 0, None), "+0");
        assert_eq!(format_dx_snr(&SpotSource::Both, 12, Some(12)), "+12");
    }

    #[test]
    fn need_marker_priority_and_stacking() {
        // Plain station — no marker.
        assert_eq!(need_marker(false, false, false), "");
        // Needed (non-ATNO) → '+'.
        assert_eq!(need_marker(false, true, false), "+");
        // ATNO → '!' and beats the '+' even if needed is also set.
        assert_eq!(need_marker(true, true, false), "!");
        // Notable stacks after the need marker.
        assert_eq!(need_marker(true, true, true), "!★");
        assert_eq!(need_marker(false, true, true), "+★");
        // Notable-only.
        assert_eq!(need_marker(false, false, true), "★");
    }

    #[test]
    fn watchlist_glyph_appears_when_watchlisted() {
        // Mirrors the band_needed precedent: the glyph is additive to the
        // need_marker cluster, not folded into need_marker itself (keeps
        // need_marker's existing signature/tests untouched).
        let mut station = make_test_station("JA1ABC");
        station.watchlisted = true;
        let call_display = format_call_display(&station, false);
        assert!(
            call_display.contains('◇'),
            "expected watchlist glyph in {call_display}"
        );
    }

    #[test]
    fn watchlist_glyph_absent_when_not_watchlisted() {
        let station = make_test_station("W1XYZ");
        let call_display = format_call_display(&station, false);
        assert!(!call_display.contains('◇'));
    }

    // --- PAN-85: Entity cell state suffix ---

    #[test]
    fn entity_cell_appends_known_state_for_us_station() {
        let s = make_test_station("W5ABC"); // no entity_name -> offline resolver -> United States
        assert_eq!(entity_cell_text(&s, Some("AR")), "United States - AR");
    }

    #[test]
    fn entity_cell_without_state_is_entity_alone() {
        let s = make_test_station("W5ABC");
        assert_eq!(entity_cell_text(&s, None), "United States");
    }

    #[test]
    fn entity_cell_never_suffixes_a_non_us_entity() {
        let s = make_test_station("JA1ABC");
        assert_eq!(entity_cell_text(&s, Some("AR")), "Japan");
    }

    #[test]
    fn entity_cell_prefers_cqdx_entity_name_and_still_suffixes() {
        let mut s = make_test_station("W5ABC");
        s.entity_name = Some("United States".into());
        assert_eq!(entity_cell_text(&s, Some("ar")), "United States - AR");
    }

    #[test]
    fn entity_cell_unresolvable_stays_dashes() {
        let s = make_test_station("<...>");
        assert_eq!(entity_cell_text(&s, Some("AR")), "---");
    }

    fn make_test_station(call: &str) -> crate::app::DxStation {
        crate::app::DxStation {
            call_sign: call.to_string(),
            grid_square: None,
            frequency: 14.074,
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
}
