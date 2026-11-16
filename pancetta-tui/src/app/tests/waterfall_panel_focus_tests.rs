// PAN-389. The Monitor waterfall as an app panel: Tab order, the palette
// and gain seeded from config, and click-to-focus.
use super::tests::{fixture_app, left_click, operate_panel_rect};
use super::*;

/// PAN-389: Monitor's Tab order is the on-screen top-to-bottom order,
/// the waterfall above Band Activity.
#[test]
fn monitor_tab_order_is_waterfall_then_band_activity() {
    assert_eq!(
        ActivePanel::tab_order(crate::view::ActiveView::Monitor),
        &[ActivePanel::Waterfall, ActivePanel::BandActivity]
    );
}

/// PAN-389: the TUI seeds the waterfall palette and gain from
/// `config.ui.waterfall`.
#[tokio::test]
async fn app_seeds_waterfall_settings_from_config() {
    let mut config = Config::default();
    config.ui.waterfall = pancetta_config::ui::WaterfallDisplayConfig {
        palette: pancetta_config::ui::WaterfallPalette::Viridis,
        gain_db: 6.0,
    };
    let app = App::new(config, None).await.unwrap();
    assert_eq!(
        app.waterfall_palette,
        pancetta_config::ui::WaterfallPalette::Viridis
    );
    assert_eq!(app.waterfall_gain_db, 6.0);
}

/// PAN-389: a left click inside the Monitor waterfall focuses it and
/// sends nothing.
#[tokio::test]
async fn click_on_monitor_waterfall_focuses_it() {
    let mut app = fixture_app().await;
    app.terminal_size = (120, 40);
    app.active_view = crate::view::ActiveView::Monitor;
    app.active_panel = ActivePanel::BandActivity;

    let rect = operate_panel_rect(&app, ActivePanel::Waterfall);
    let x = rect.x + rect.width / 2;
    let y = rect.y + rect.height / 2;

    let cmd = app.handle_mouse_event(left_click(x, y)).await.unwrap();
    assert!(cmd.is_none(), "click-to-focus never sends a TuiCommand");
    assert_eq!(app.active_panel, ActivePanel::Waterfall);
}
