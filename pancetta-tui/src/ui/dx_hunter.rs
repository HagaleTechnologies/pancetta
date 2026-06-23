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

pub fn render_dx_hunter(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::DxHunter);
    let block = create_panel_block("DX Hunter", is_active, app);

    // Prepare table data
    // "Freq" dropped — every DX Hunter spot is on the current band's FT8 dial,
    // so the column was always identical. Replaced with the DXCC "Entity" the
    // operator actually wants to scan.
    let header_cells = [
        "Src", "Call", "Entity", "Grid", "SNR", "Rarity", "Rpt", "Last", "Pri",
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
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
        .map(|station| create_dx_row(station, app))
        .collect();

    // If no DX stations, show placeholder
    if rows.is_empty() {
        rows.push(Row::new(
            [
                Cell::from(""),
                Cell::from("No DX"),
                Cell::from("stations"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ]
            .iter()
            .map(|c| {
                c.clone()
                    .style(Style::default().fg(app.theme.muted_color()))
            }),
        ));
    }

    let widths = [
        Constraint::Length(4),  // Src
        Constraint::Length(10), // Call (★ + up to ~8 chars)
        Constraint::Min(8),     // Entity (flexes into the freed Freq space)
        Constraint::Length(4),  // Grid
        Constraint::Length(4),  // SNR
        Constraint::Length(7),  // Rarity
        Constraint::Length(3),  // Rpt
        Constraint::Length(5),  // Last
        Constraint::Length(4),  // Pri
    ];

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
        .highlight_symbol("▶ ");

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

fn create_dx_row<'a>(station: &'a DxStation, app: &App) -> Row<'a> {
    // Source indicator
    let src_str = station.source.to_string();
    let src_style = match station.source {
        SpotSource::Local => Style::default().fg(app.theme.success_color()),
        SpotSource::Network => Style::default().fg(app.theme.accent_color()),
        SpotSource::Both => Style::default().fg(ratatui::style::Color::Cyan),
    };

    // Callsign with notable prefix
    let call_display = if station.is_notable {
        format!("★{}", station.call_sign)
    } else {
        station.call_sign.clone()
    };

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
    } else if station.is_notable {
        Style::default()
            .fg(ratatui::style::Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if station.worked_before {
        Style::default().fg(app.theme.muted_color())
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

    let grid_str = station.grid_square.as_deref().unwrap_or("---").to_string();
    // Prefer cqdx's authoritative entity name; fall back to the offline
    // prefix resolver for local decodes (which carry no cqdx metadata), so the
    // DXCC column isn't "---" for every locally-heard station.
    let entity_str = station
        .entity_name
        .clone()
        .or_else(|| crate::dxcc::entity_for_callsign(&station.call_sign).map(str::to_string))
        .unwrap_or_else(|| "---".to_string());
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
        score if score > 100 => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        score if score > 50 => Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD),
        _ => dim,
    };

    Row::new([
        Cell::from(src_str).style(src_style),
        Cell::from(call_display).style(call_style),
        Cell::from(entity_str).style(Style::default().fg(app.theme.accent_color())),
        Cell::from(grid_str).style(dim),
        Cell::from(snr_str).style(snr_style),
        Cell::from(rarity_str).style(rarity_style),
        Cell::from(rpt_str).style(dim),
        Cell::from(last_str).style(dim),
        Cell::from(pri_str).style(priority_style),
    ])
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

/// Calculate DX priority score based on various factors
pub fn calculate_dx_priority(
    station: &DxStation,
    _our_grid: &str,
    worked_before: bool,
    is_new_dxcc: bool,
    is_new_band: bool,
) -> u32 {
    let mut score = 0u32;

    // Base score from SNR (0-30 points)
    if station.snr > 0 {
        score += (station.snr as u32).min(30);
    }

    // Distance bonus (0-50 points)
    if let Some(distance) = station.distance {
        if distance > 10000.0 {
            score += 50; // Very long distance
        } else if distance > 5000.0 {
            score += 30;
        } else if distance > 1000.0 {
            score += 10;
        }
    }

    // DXCC bonuses
    if is_new_dxcc {
        score += 200; // New country
    }

    if is_new_band {
        score += 100; // New band
    }

    // Rarity-tier-based DX bonus
    match station.rarity_tier.as_deref() {
        Some("legendary") => score += 200,
        Some("very_rare") => score += 150,
        Some("rare") => score += 100,
        Some("uncommon") => score += 50,
        _ => {
            if is_rare_dx_fallback(&station.call_sign) {
                score += 150;
            }
        }
    }

    // Penalty for already worked
    if worked_before {
        score = score.saturating_sub(50);
    }

    // Recency bonus (more recent = higher priority)
    let age_minutes = chrono::Utc::now()
        .signed_duration_since(station.last_seen)
        .num_minutes();

    if age_minutes < 5 {
        score += 20;
    } else if age_minutes < 15 {
        score += 10;
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn test_calculate_dx_priority() {
        let station = DxStation {
            call_sign: "JA1ABC".to_string(),
            grid_square: Some("PM95".to_string()),
            frequency: 14.074,
            mode: "FT8".to_string(),
            last_seen: chrono::Utc::now(),
            snr: 15,
            distance: Some(8000.0),
            bearing: Some(45.0),
            worked_before: false,
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
        };

        let score = calculate_dx_priority(&station, "FN20", false, true, false);
        assert!(score > 100); // Should have good score for new DXCC + good SNR + distance
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
}
