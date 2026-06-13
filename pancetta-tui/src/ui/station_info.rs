use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Gauge, Paragraph},
    Frame,
};

use super::create_panel_block;
use crate::app::{ActivePanel, App};

pub fn render_station_info(f: &mut Frame<'_>, area: Rect, app: &App) -> Result<()> {
    let is_active = matches!(app.active_panel, ActivePanel::StationInfo);
    let block = create_panel_block("Station Info", is_active, app);

    // Split area into sections
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // Station details
            Constraint::Length(3), // Operating parameters
            Constraint::Length(2), // Audio monitoring
            Constraint::Length(2), // Autonomous status
            Constraint::Min(1),    // Equipment/antenna info
        ])
        .split(block.inner(area));

    // Render the block border
    f.render_widget(block, area);

    // Station Details
    render_station_details(f, chunks[0], app);

    // Operating Parameters
    render_operating_parameters(f, chunks[1], app);

    // Audio Monitoring Status
    render_audio_status(f, chunks[2], app);

    // Autonomous Operator Status
    render_autonomous_status(f, chunks[3], app);

    // Equipment Information
    render_equipment_info(f, chunks[4], app);

    Ok(())
}

fn render_station_details(f: &mut Frame<'_>, area: Rect, app: &App) {
    let station = &app.station_info;

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "Call Sign: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                &station.call_sign,
                Style::default()
                    .fg(app.theme.success_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            Span::styled(
                "Grid Square: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                &station.grid_square,
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Power: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                format!("{} W", station.power),
                Style::default()
                    .fg(app.theme.warning_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            Span::styled(
                "Antenna: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                &station.antenna,
                Style::default().fg(app.theme.foreground_color()),
            ),
        ]),
        Line::from(vec![
            Span::styled("Rig: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                &station.rig,
                Style::default().fg(app.theme.foreground_color()),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_operating_parameters(f: &mut Frame<'_>, area: Rect, app: &App) {
    let station = &app.station_info;

    // Get current band info
    let band_info = app
        .config
        .get_current_band(station.operating_frequency)
        .map(|b| b.name.as_str())
        .unwrap_or("Unknown");

    // Build frequency line with optional radio delta
    let mut freq_spans = vec![
        Span::styled("Freq: ", Style::default().fg(app.theme.foreground_color())),
        Span::styled(
            app.config
                .ui
                .frequency_format
                .format_frequency(station.operating_frequency),
            Style::default()
                .fg(app.theme.warning_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("Band: ", Style::default().fg(app.theme.foreground_color())),
        Span::styled(
            band_info,
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  [+/-]"),
    ];

    // Show radio frequency delta if known
    if let Some(delta_khz) = app.frequency_delta_khz() {
        if delta_khz.abs() > 2.0 {
            // More than 500 Hz off
            freq_spans.push(Span::raw("  "));
            freq_spans.push(Span::styled(
                format!(
                    "RADIO: {:.3} MHz ({:+.1} kHz)",
                    app.radio_frequency.unwrap_or(0.0),
                    delta_khz
                ),
                Style::default()
                    .fg(app.theme.error_color())
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    let lines = vec![
        Line::from(freq_spans),
        Line::from(vec![
            Span::styled("Mode: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                &station.mode,
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            Span::styled(
                "Status: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                if app.is_monitoring {
                    "MONITORING"
                } else {
                    "STANDBY"
                },
                Style::default()
                    .fg(if app.is_monitoring {
                        app.theme.success_color()
                    } else {
                        app.theme.muted_color()
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);
}

fn render_audio_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    // Split into two sections: level meter and device info
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Audio level gauge
    let level_ratio = (app.audio_level as f64).clamp(0.0, 1.0);
    let level_color = get_audio_level_color(app.audio_level, &app.theme);

    let level_gauge = Gauge::default()
        .block(ratatui::widgets::Block::default().title("Audio Level"))
        .gauge_style(Style::default().fg(level_color))
        .ratio(level_ratio)
        .label(format!("{:.1}%", app.audio_level * 100.0));

    f.render_widget(level_gauge, chunks[0]);

    // Device information
    let device_text = app.audio_device.as_deref().unwrap_or("Default");
    let sample_rate = app.config.audio.sample_rate;

    // Rig S-meter (real STRENGTH reads from hamlib, Batch 95). "---"
    // when no rig is connected or the last reading went stale — we
    // never synthesize a value here.
    let s_meter_text = app.s_meter_display().unwrap_or_else(|| "---".to_string());

    let device_lines = vec![
        Line::from(vec![
            Span::styled(
                "Device: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(device_text, Style::default().fg(app.theme.accent_color())),
        ]),
        Line::from(vec![
            Span::styled("Rate: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                format!("{} Hz", sample_rate),
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::raw("  "),
            Span::styled(
                "S-meter: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(s_meter_text, Style::default().fg(app.theme.accent_color())),
        ]),
    ];

    let device_paragraph = Paragraph::new(device_lines);
    f.render_widget(device_paragraph, chunks[1]);
}

fn render_equipment_info(f: &mut Frame<'_>, area: Rect, app: &App) {
    let station = &app.station_info;

    // Calculate coordinates from grid square (basic Maidenhead conversion)
    let (lat, lon) = grid_to_coordinates(&station.grid_square);

    let lines = vec![Line::from(vec![
        Span::styled(
            "Equipment: ",
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::styled(
            &station.rig,
            Style::default().fg(app.theme.foreground_color()),
        ),
        Span::raw(" + "),
        Span::styled(
            &station.antenna,
            Style::default().fg(app.theme.foreground_color()),
        ),
    ])];

    // Add coordinates if we have room and they're enabled
    let mut all_lines = lines;
    if area.height > 1 && app.config.ui.show_coordinates {
        all_lines.push(Line::from(vec![
            Span::styled(
                "Position: ",
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::styled(
                format!(
                    "{:.2}°{}, {:.2}°{}",
                    lat.abs(),
                    if lat >= 0.0 { "N" } else { "S" },
                    lon.abs(),
                    if lon >= 0.0 { "E" } else { "W" },
                ),
                Style::default().fg(app.theme.muted_color()),
            ),
        ]));
    }

    // Add statistics if we have room
    if area.height > 2 {
        let total_messages = app.decoded_messages.len();
        let dx_stations = app.dx_stations.len();

        all_lines.push(Line::from(vec![
            Span::styled("Stats: ", Style::default().fg(app.theme.foreground_color())),
            Span::styled(
                format!("{} msgs", total_messages),
                Style::default().fg(app.theme.accent_color()),
            ),
            Span::raw(", "),
            Span::styled(
                format!("{} DX", dx_stations),
                Style::default().fg(app.theme.accent_color()),
            ),
        ]));
    }

    let paragraph = Paragraph::new(all_lines);
    f.render_widget(paragraph, area);
}

fn render_autonomous_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(ref status) = app.autonomous_status {
        let state_color = if !status.enabled {
            app.theme.muted_color()
        } else if status.state == "Paused" {
            app.theme.warning_color()
        } else {
            app.theme.success_color()
        };

        let parity_str = status.slot_parity.as_deref().unwrap_or("--");

        let lines = vec![Line::from(vec![
            Span::styled(
                "[AUTO] ",
                Style::default()
                    .fg(app.theme.accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &status.state,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | Slot: "),
            Span::styled(
                parity_str,
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::raw(" | Listen: "),
            Span::styled(
                &status.listen_counter,
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::raw(" | QSOs: "),
            Span::styled(
                format!("{}/{}", status.active_qsos, status.max_qsos),
                Style::default().fg(app.theme.foreground_color()),
            ),
            Span::raw(" | "),
            Span::styled(
                &status.band_name,
                Style::default().fg(app.theme.accent_color()),
            ),
        ])];

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, area);
    } else {
        let lines = vec![Line::from(vec![
            Span::styled("[AUTO] ", Style::default().fg(app.theme.muted_color())),
            Span::styled("Disabled", Style::default().fg(app.theme.muted_color())),
        ])];
        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, area);
    }
}

fn get_audio_level_color(level: f32, theme: &crate::config::Theme) -> ratatui::style::Color {
    match level {
        level if level > 0.9 => theme.error_color(), // Overload
        level if level > 0.7 => theme.warning_color(), // High but OK
        level if level > 0.1 => theme.success_color(), // Good level
        level if level > 0.01 => theme.warning_color(), // Low
        _ => theme.muted_color(),                    // No signal
    }
}

/// Convert Maidenhead grid square to approximate lat/lon coordinates
fn grid_to_coordinates(grid: &str) -> (f64, f64) {
    if grid.len() < 4 {
        return (0.0, 0.0);
    }

    let grid_upper = grid.to_uppercase();
    let chars: Vec<char> = grid_upper.chars().collect();

    // Field (first two letters)
    let field_lon = (chars[0] as u8 - b'A') as f64 * 20.0 - 180.0;
    let field_lat = (chars[1] as u8 - b'A') as f64 * 10.0 - 90.0;

    // Square (next two digits)
    let square_lon = chars[2].to_digit(10).unwrap_or(0) as f64 * 2.0;
    let square_lat = chars[3].to_digit(10).unwrap_or(0) as f64 * 1.0;

    let lon = field_lon + square_lon + 1.0; // Add 1 for center of square
    let lat = field_lat + square_lat + 0.5; // Add 0.5 for center of square

    (lat, lon)
}

/// Calculate distance between two grid squares (rough approximation)
pub fn calculate_distance(grid1: &str, grid2: &str) -> Option<f64> {
    let (lat1, lon1) = grid_to_coordinates(grid1);
    let (lat2, lon2) = grid_to_coordinates(grid2);

    if lat1 == 0.0 && lon1 == 0.0 || lat2 == 0.0 && lon2 == 0.0 {
        return None;
    }

    // Haversine formula (simplified)
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();

    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().atan2((1.0_f64 - a).sqrt());
    let distance = 6371.0 * c; // Earth radius in km

    Some(distance)
}

/// Calculate bearing between two grid squares
pub fn calculate_bearing(grid1: &str, grid2: &str) -> Option<f64> {
    let (lat1, lon1) = grid_to_coordinates(grid1);
    let (lat2, lon2) = grid_to_coordinates(grid2);

    if lat1 == 0.0 && lon1 == 0.0 || lat2 == 0.0 && lon2 == 0.0 {
        return None;
    }

    let _dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();

    let y = dlon.sin() * lat2_rad.cos();
    let x = lat1_rad.cos() * lat2_rad.sin() - lat1_rad.sin() * lat2_rad.cos() * dlon.cos();

    let bearing = y.atan2(x).to_degrees();
    Some((bearing + 360.0) % 360.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_to_coordinates() {
        let (lat, lon) = grid_to_coordinates("FN20");
        assert!((lat - 40.5).abs() < 1.0); // Approximate latitude
        assert!((lon - (-74.0)).abs() < 2.0); // Approximate longitude
    }

    #[test]
    fn test_calculate_distance() {
        let distance = calculate_distance("FN20", "EM79").unwrap();
        assert!(distance > 500.0 && distance < 2000.0); // Rough check (~950 km)
    }

    #[test]
    fn test_audio_level_color() {
        use crate::config::Theme;
        let theme = Theme::Dark;

        assert_eq!(get_audio_level_color(0.95, &theme), theme.error_color());
        assert_eq!(get_audio_level_color(0.5, &theme), theme.success_color());
        assert_eq!(get_audio_level_color(0.001, &theme), theme.muted_color());
    }
}
