// PAN-388: the footer shows the view name and the focused panel's hints.
use super::view_render_tests::{default_app, render_app, row_text};
use super::*;

/// Band Activity's footer tokens that must not appear on DX Hunter.
const BAND_ACTIVITY_TOKENS: [&str; 4] = ["Arrows:TX", "=/-:Band", "g:TX-policy", "d:Audio"];

/// PAN-388 scenario 1: DX Hunter's footer lists its own actions.
#[tokio::test]
async fn footer_dx_hunter_lists_its_actions_not_band_activitys() {
    let mut app = default_app().await;
    app.active_view = crate::view::ActiveView::Operate;
    app.active_panel = ActivePanel::DxHunter;
    for (w, h, expected) in [
        (
            80,
            24,
            "Operate | Space:Call | Enter:Details | W:Watchlist | L:Lookup | ?:Help | q:Quit",
        ),
        (
            100,
            30,
            "Operate view | Space:Call | Enter:Details | W:Watchlist | L:Lookup | H:Hound | ?:Help | q:Quit",
        ),
        (
            132,
            40,
            "Operate view | Space:Call | Enter:Details | W:Watchlist | L:Lookup | H:Hound | v:View | ?:Help | q:Quit",
        ),
    ] {
        let buf = render_app(&app, w, h);
        let row = row_text(&buf, h - 1);
        let row = row.trim_end();
        assert_eq!(row, expected, "{w}x{h}");
        for token in BAND_ACTIVITY_TOKENS {
            assert!(!row.contains(token), "{w}x{h}: {token} in {row}");
        }
    }
}

/// PAN-388: every view, panel and width keeps the view name, `?:Help`
/// and `q:Quit`, and never overflows the row.
#[tokio::test]
async fn footer_pins_view_help_and_quit_at_every_width() {
    use crate::view::ActiveView;
    let mut app = default_app().await;
    let mut renders = 0;
    for view in [
        ActiveView::Operate,
        ActiveView::Hunt,
        ActiveView::Run,
        ActiveView::Monitor,
    ] {
        for panel in [
            ActivePanel::BandActivity,
            ActivePanel::QsoStatus,
            ActivePanel::Callers,
            ActivePanel::DxHunter,
            ActivePanel::TxPlacement,
        ] {
            app.active_view = view;
            app.active_panel = panel;
            for w in 80..=200u16 {
                let buf = render_app(&app, w, 24);
                let row = row_text(&buf, 23);
                let row = row.trim_end();
                let ctx = format!("{view:?}/{panel:?} at {w}: {row}");
                assert!(
                    row.starts_with(crate::presentation::view_name(view)),
                    "{ctx}"
                );
                assert!(row.ends_with("?:Help | q:Quit"), "{ctx}");
                assert!(row.chars().count() <= w as usize, "{ctx}");
                if panel == ActivePanel::DxHunter {
                    for token in ["Space:Call", "Enter:Details", "W:Watchlist", "L:Lookup"] {
                        assert!(row.contains(token), "{ctx}: missing {token}");
                    }
                    for token in BAND_ACTIVITY_TOKENS {
                        assert!(!row.contains(token), "{ctx}: {token}");
                    }
                }
                renders += 1;
            }
        }
    }
    assert_eq!(renders, 2420);
}

/// PAN-388 scenario 2: the footer names the current view.
#[tokio::test]
async fn footer_names_every_view() {
    use crate::view::ActiveView;
    let mut app = default_app().await;
    app.active_panel = ActivePanel::BandActivity;
    for (view, name) in [
        (ActiveView::Operate, "Operate view | "),
        (ActiveView::Hunt, "Hunt view | "),
        (ActiveView::Run, "Run view | "),
        (ActiveView::Monitor, "Monitor view | "),
    ] {
        app.active_view = view;
        let buf = render_app(&app, 100, 30);
        let row = row_text(&buf, 29);
        assert!(row.starts_with(name), "{view:?}: {row}");
    }
}

/// PAN-388: the other Operate panels show their own hints.
#[tokio::test]
async fn footer_strips_per_panel() {
    let mut app = default_app().await;
    app.active_view = crate::view::ActiveView::Operate;
    for (panel, w, h, expected) in [
        (
            ActivePanel::QsoStatus,
            80,
            24,
            "Operate view | Up/Down:Select | r:Resend | k:Abort | v:View | ?:Help | q:Quit",
        ),
        (
            ActivePanel::Callers,
            80,
            24,
            "Operate | Enter:Reply | Left/Right:Step | Space:Call/Reply | ?:Help | q:Quit",
        ),
        (
            ActivePanel::TxPlacement,
            80,
            24,
            "Operate view | Left/Right:Pick | Enter:Park | v:View | ?:Help | q:Quit",
        ),
        (
            ActivePanel::QsoStatus,
            100,
            30,
            "Operate view | Up/Down:Select | r:Resend | k:Abort | v:View | ?:Help | q:Quit",
        ),
        (
            ActivePanel::Callers,
            100,
            30,
            "Operate view | Enter:Reply | Left/Right:Step | Space:Call/Reply | v:View | ?:Help | q:Quit",
        ),
        (
            ActivePanel::TxPlacement,
            100,
            30,
            "Operate view | Left/Right:Pick | Enter:Park | v:View | ?:Help | q:Quit",
        ),
    ] {
        app.active_panel = panel;
        let buf = render_app(&app, w, h);
        let row = row_text(&buf, h - 1);
        let row = row.trim_end();
        assert_eq!(row, expected, "{panel:?} at {w}x{h}");
        if panel == ActivePanel::TxPlacement {
            assert!(!row.contains("Space:"), "{row}");
        }
    }
}
