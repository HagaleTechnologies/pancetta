use anyhow::Result;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Cell, Row, Table, TableState},
    Frame,
};

use super::{create_panel_block, format_bearing, format_distance, format_time_ago};
use crate::app::{ActivePanel, App, DxStation};

pub fn render_dx_hunter(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::DxHunter);
    let block = create_panel_block("DX Hunter", is_active, app);

    // Prepare table data
    let header_cells = ["Call", "Grid", "Freq", "SNR", "Dist", "Bear", "Last", "Pri"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            )
        });

    let header = Row::new(header_cells).height(1).bottom_margin(0);

    // Sort DX stations by priority score (highest first)
    let mut dx_list: Vec<&DxStation> = app.dx_stations.values().collect();
    dx_list.sort_by(|a, b| b.priority_score.cmp(&a.priority_score));

    // Convert to table rows
    let mut rows: Vec<Row> = dx_list
        .iter()
        .skip(app.dx_hunter_scroll)
        .take((area.height as usize).saturating_sub(4)) // Account for borders and header
        .map(|station| create_dx_row(station, app))
        .collect();

    // If no DX stations, show placeholder
    if rows.is_empty() {
        rows.push(Row::new(
            [
                Cell::from(""),
                Cell::from(""),
                Cell::from("No DX"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from("stations"),
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
        Constraint::Length(8), // Call
        Constraint::Length(4), // Grid
        Constraint::Length(7), // Freq
        Constraint::Length(4), // SNR
        Constraint::Length(6), // Dist
        Constraint::Length(4), // Bear
        Constraint::Length(5), // Last
        Constraint::Length(3), // Pri
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .style(Style::default().fg(app.theme.foreground_color()));

    // Create table state for potential selection
    let mut table_state = TableState::default();
    if is_active && !dx_list.is_empty() {
        table_state.select(Some(app.dx_hunter_scroll));
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
    let call_str = &station.call_sign;
    let grid_str = station.grid_square.as_deref().unwrap_or("---");
    let freq_str = format!("{:.3}", station.frequency);
    let snr_str = format!("{:+}", station.snr);
    let dist_str = format_distance(station.distance);
    let bear_str = format_bearing(station.bearing);
    let last_str = format_time_ago(station.last_seen);
    let pri_str = format!("{}", station.priority_score);

    // Color coding based on various factors
    let call_style = if station.worked_before {
        Style::default().fg(app.theme.muted_color())
    } else if is_rare_dx(&station.call_sign) {
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

    let snr_style = Style::default().fg(get_snr_color(station.snr, &app.theme));

    let priority_style = match station.priority_score {
        score if score > 100 => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        score if score > 50 => Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD),
        _ => Style::default().fg(app.theme.foreground_color()),
    };

    Row::new([
        Cell::from(call_str.as_str()).style(call_style),
        Cell::from(grid_str).style(Style::default().fg(app.theme.foreground_color())),
        Cell::from(freq_str).style(Style::default().fg(app.theme.accent_color())),
        Cell::from(snr_str).style(snr_style),
        Cell::from(dist_str).style(Style::default().fg(app.theme.foreground_color())),
        Cell::from(bear_str).style(Style::default().fg(app.theme.foreground_color())),
        Cell::from(last_str).style(Style::default().fg(app.theme.muted_color())),
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

/// Check if a callsign represents rare DX
fn is_rare_dx(call_sign: &str) -> bool {
    // Basic check for some rare prefixes
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

    // Rare DX bonus
    if is_rare_dx(&station.call_sign) {
        score += 150;
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
    fn test_is_rare_dx() {
        assert!(is_rare_dx("3Y0X"));
        assert!(is_rare_dx("VP8STI"));
        assert!(!is_rare_dx("W1ABC"));
        assert!(!is_rare_dx("G0XYZ"));
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
        };

        let score = calculate_dx_priority(&station, "FN20", false, true, false);
        assert!(score > 100); // Should have good score for new DXCC + good SNR + distance
    }
}
