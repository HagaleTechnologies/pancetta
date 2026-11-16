// PAN-389. The Monitor waterfall panel renders the configured palette and
// gain, shows its key hint and highlighted border only when focused, zooms,
// and a gain key press moves the rendered floor.
use super::view_render_tests::{buffer_contains, row_text};
use super::*;
use ratatui::{backend::TestBackend, Terminal};

// ---- PAN-389: waterfall palette, gain and focus ----------------------

/// A Monitor-view app whose waterfall holds `rows` uniform 0.0 rows.
async fn monitor_app(waterfall: pancetta_config::ui::WaterfallDisplayConfig) -> crate::app::App {
    let mut config = crate::config::Config::default();
    config.ui.waterfall = waterfall;
    let mut app = crate::app::App::new(config, None).await.unwrap();
    app.active_view = crate::view::ActiveView::Monitor;
    app.active_panel = crate::app::ActivePanel::BandActivity;
    app.color_capability = crate::app::ColorCapability::TwoFiftySix;
    app.waterfall_data = vec![vec![0.0; 64]; 60];
    app
}

fn draw_app(app: &crate::app::App) -> ratatui::buffer::Buffer {
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| draw(f, app).unwrap()).unwrap();
    term.backend().buffer().clone()
}

/// The waterfall panel rect `draw` lays out for `app` at 120x40.
fn waterfall_rect(app: &crate::app::App) -> Rect {
    view_rects(
        app.active_view,
        app.zoomed,
        app.active_panel,
        content_area(Rect::new(0, 0, 120, 40)),
        false,
    )
    .into_iter()
    .find(|(p, _)| *p == crate::app::ActivePanel::Waterfall)
    .map(|(_, r)| r)
    .expect("waterfall rect")
}

/// A data cell well left of the TX cursor (row 0 inside the border is
/// the occupancy strip, so the first data row is two below the top).
fn floor_cell(buf: &ratatui::buffer::Buffer, rect: Rect) -> ratatui::style::Color {
    buf[(rect.x + 5, rect.y + 2)].fg
}

/// The 256-colour ramp entry a floor cell should show (PAN-387: the
/// colour values live in theme.rs and are pinned there).
fn floor_ramp(
    palette: pancetta_config::ui::WaterfallPalette,
    intensity: f32,
) -> ratatui::style::Color {
    theme::waterfall_ramp(palette, crate::app::ColorCapability::TwoFiftySix, intensity)
}

fn wf(
    palette: pancetta_config::ui::WaterfallPalette,
    gain_db: f32,
) -> pancetta_config::ui::WaterfallDisplayConfig {
    pancetta_config::ui::WaterfallDisplayConfig { palette, gain_db }
}

#[tokio::test]
async fn monitor_waterfall_uses_configured_palette_and_gain() {
    use pancetta_config::ui::WaterfallPalette::Viridis;
    let app = monitor_app(wf(pancetta_config::ui::WaterfallPalette::Viridis, 6.0)).await;
    let buf = draw_app(&app);
    let rect = waterfall_rect(&app);
    let title = row_text(&buf, rect.y);
    assert!(
        title.contains("Waterfall: TX 1500 Hz · viridis · gain +6 dB"),
        "{title}"
    );
    assert!(!title.contains("[/]"), "{title}");
    assert_eq!(floor_cell(&buf, rect), floor_ramp(Viridis, 0.5));
}

#[tokio::test]
async fn waterfall_title_shows_key_hint_only_when_focused() {
    let mut app = monitor_app(Default::default()).await;
    let rect = waterfall_rect(&app);
    let unfocused = row_text(&draw_app(&app), rect.y);
    assert!(
        unfocused.contains(" Waterfall: TX 1500 Hz · classic · gain 0 dB "),
        "{unfocused}"
    );
    assert!(!unfocused.contains("{ } gain"), "{unfocused}");

    app.active_panel = crate::app::ActivePanel::Waterfall;
    let focused = row_text(&draw_app(&app), rect.y);
    assert!(
        focused.contains(" Waterfall: TX 1500 Hz · classic · gain 0 dB · { } gain · b B palette "),
        "{focused}"
    );
}

#[tokio::test]
async fn focused_waterfall_border_is_highlighted() {
    let mut app = monitor_app(Default::default()).await;
    let rect = waterfall_rect(&app);

    let buf = draw_app(&app);
    let corner = &buf[(rect.x, rect.y)];
    assert_eq!(corner.fg, theme::fg_color(app.theme.border));
    assert!(!corner.modifier.contains(Modifier::BOLD));

    app.active_panel = crate::app::ActivePanel::Waterfall;
    let buf = draw_app(&app);
    let corner = &buf[(rect.x, rect.y)];
    assert_eq!(corner.fg, theme::fg_color(app.theme.focus));
    assert!(corner.modifier.contains(Modifier::BOLD));
}

#[tokio::test]
async fn zoomed_waterfall_fills_content_area() {
    let mut app = monitor_app(Default::default()).await;
    app.active_panel = crate::app::ActivePanel::Waterfall;
    app.zoomed = true;
    let buf = draw_app(&app);
    let zoom_rect = compute_zoom_rects(content_area(Rect::new(0, 0, 120, 40))).1;
    assert_eq!(waterfall_rect(&app), zoom_rect);
    assert!(
        row_text(&buf, zoom_rect.y).contains("Waterfall: TX"),
        "the zoomed panel's title row is the waterfall's"
    );
    assert!(!buffer_contains(&buf, "Band Activity"));
    // Bottom border of the zoomed block is the content area's last row.
    assert_eq!(
        buf[(zoom_rect.x, zoom_rect.y + zoom_rect.height - 1)].symbol(),
        "└"
    );
}

#[tokio::test]
async fn gain_key_press_moves_the_rendered_floor() {
    use pancetta_config::ui::WaterfallPalette::Classic;
    let mut app = monitor_app(Default::default()).await;
    app.active_panel = crate::app::ActivePanel::Waterfall;
    let rect = waterfall_rect(&app);
    assert_eq!(floor_cell(&draw_app(&app), rect), floor_ramp(Classic, 0.0));
    assert_eq!(app.adjust_waterfall_gain(1.0), Some(1.0));
    assert_ne!(floor_ramp(Classic, 0.0), floor_ramp(Classic, 1.0 / 12.0));
    assert_eq!(
        floor_cell(&draw_app(&app), rect),
        floor_ramp(Classic, 1.0 / 12.0)
    );
}
