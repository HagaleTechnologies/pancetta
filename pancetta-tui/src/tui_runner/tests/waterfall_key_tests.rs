// PAN-389. Waterfall gain and palette keys in the Monitor view: one dB steps,
// clamps at the limits, palette cycling, the focus hint outside the panel,
// and the zoomed waterfall in any view.
use super::key_tests::{key, key_shift, make_runner};
use super::*;

// ---- PAN-389: waterfall gain and palette keys --------------------------

/// Focus the Monitor waterfall on a fresh runner's app.
async fn focus_waterfall(app: &Arc<RwLock<App>>) {
    let mut a = app.write().await;
    a.active_view = crate::view::ActiveView::Monitor;
    a.active_panel = crate::app::ActivePanel::Waterfall;
    a.color_capability = crate::app::ColorCapability::TwoFiftySix;
}

#[tokio::test]
async fn waterfall_gain_keys_step_one_db_and_emit_set_waterfall_gain() {
    let (mut r, cmd_rx, app) = make_runner().await;
    focus_waterfall(&app).await;
    assert_eq!(app.read().await.waterfall_gain_db, 0.0);

    r.handle_key_event(key('}')).await.unwrap();
    assert_eq!(app.read().await.waterfall_gain_db, 1.0);
    assert_eq!(app.read().await.status_message(), "Waterfall gain +1 dB");
    assert!(
        matches!(cmd_rx.try_recv(), Ok(TuiCommand::SetWaterfallGain { gain_db }) if gain_db == 1.0)
    );
    assert!(cmd_rx.try_recv().is_err(), "exactly one command per press");

    r.handle_key_event(key('{')).await.unwrap();
    assert_eq!(app.read().await.waterfall_gain_db, 0.0);
    assert_eq!(app.read().await.status_message(), "Waterfall gain 0 dB");
    assert!(
        matches!(cmd_rx.try_recv(), Ok(TuiCommand::SetWaterfallGain { gain_db }) if gain_db == 0.0)
    );

    for _ in 0..3 {
        r.handle_key_event(key('{')).await.unwrap();
    }
    assert_eq!(app.read().await.waterfall_gain_db, -3.0);
    assert_eq!(app.read().await.status_message(), "Waterfall gain -3 dB");

    app.write().await.waterfall_gain_db = 6.0;
    r.handle_key_event(key_shift('}')).await.unwrap();
    assert_eq!(app.read().await.status_message(), "Waterfall gain +7 dB");
}

#[tokio::test]
async fn waterfall_gain_keys_clamp_at_twelve_db_without_a_command() {
    let (mut r, cmd_rx, app) = make_runner().await;
    focus_waterfall(&app).await;

    app.write().await.waterfall_gain_db = 12.0;
    r.handle_key_event(key('}')).await.unwrap();
    assert_eq!(app.read().await.waterfall_gain_db, 12.0);
    assert_eq!(
        app.read().await.status_message(),
        "Waterfall gain +12 dB (maximum)"
    );
    assert!(cmd_rx.try_recv().is_err(), "no command at the maximum");

    app.write().await.waterfall_gain_db = -12.0;
    r.handle_key_event(key('{')).await.unwrap();
    assert_eq!(app.read().await.waterfall_gain_db, -12.0);
    assert_eq!(
        app.read().await.status_message(),
        "Waterfall gain -12 dB (minimum)"
    );
    assert!(cmd_rx.try_recv().is_err(), "no command at the minimum");
}

#[tokio::test]
async fn waterfall_palette_keys_cycle_and_emit_set_waterfall_palette() {
    use pancetta_config::ui::WaterfallPalette;
    let (mut r, cmd_rx, app) = make_runner().await;
    focus_waterfall(&app).await;

    let expected = [
        (WaterfallPalette::Spectrum, "spectrum"),
        (WaterfallPalette::Thermal, "thermal"),
        (WaterfallPalette::Viridis, "viridis"),
        (WaterfallPalette::Classic, "classic"),
    ];
    for (palette, name) in expected {
        r.handle_key_event(key('b')).await.unwrap();
        assert_eq!(app.read().await.waterfall_palette, palette);
        assert_eq!(
            app.read().await.status_message(),
            format!("Waterfall palette: {name}")
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(TuiCommand::SetWaterfallPalette { palette: p }) if p == palette
        ));
    }

    r.handle_key_event(key_shift('B')).await.unwrap();
    assert_eq!(
        app.read().await.waterfall_palette,
        WaterfallPalette::Viridis
    );
    assert_eq!(
        app.read().await.status_message(),
        "Waterfall palette: viridis"
    );
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::SetWaterfallPalette {
            palette: WaterfallPalette::Viridis
        })
    ));

    // 16-colour terminal: the palette changes but is not shown.
    app.write().await.color_capability = crate::app::ColorCapability::Basic;
    r.handle_key_event(key('b')).await.unwrap();
    assert_eq!(
        app.read().await.waterfall_palette,
        WaterfallPalette::Classic
    );
    assert_eq!(
        app.read().await.status_message(),
        "Waterfall palette: classic (shown on 256-colour terminals only)"
    );
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::SetWaterfallPalette {
            palette: WaterfallPalette::Classic
        })
    ));
}

#[tokio::test]
async fn waterfall_keys_outside_the_waterfall_panel_only_hint() {
    use crate::app::ActivePanel;
    const HINT: &str = "Focus the waterfall (Monitor view, Tab) to change its gain or palette";
    for panel in [
        ActivePanel::BandActivity,
        ActivePanel::QsoStatus,
        ActivePanel::Callers,
        ActivePanel::DxHunter,
        ActivePanel::TxPlacement,
    ] {
        for ev in [key('{'), key('}'), key('b'), key_shift('B')] {
            let (mut r, cmd_rx, app) = make_runner().await;
            app.write().await.active_panel = panel;
            let (palette, gain) = {
                let a = app.read().await;
                (a.waterfall_palette, a.waterfall_gain_db)
            };
            r.handle_key_event(ev).await.unwrap();
            let a = app.read().await;
            assert_eq!(a.status_message(), HINT, "{panel:?} {ev:?}");
            assert_eq!(a.waterfall_palette, palette, "{panel:?} {ev:?}");
            assert_eq!(a.waterfall_gain_db, gain, "{panel:?} {ev:?}");
            assert!(cmd_rx.try_recv().is_err(), "{panel:?} {ev:?}: no command");
        }
    }

    // Ctrl+} and Alt+b do nothing at all, focused or not.
    for focused in [false, true] {
        for ev in [
            KeyEvent::new(KeyCode::Char('}'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
        ] {
            let (mut r, cmd_rx, app) = make_runner().await;
            if focused {
                focus_waterfall(&app).await;
            }
            let before = app.read().await.status_message();
            r.handle_key_event(ev).await.unwrap();
            let a = app.read().await;
            assert_eq!(a.status_message(), before, "{ev:?} focused={focused}");
            assert_eq!(a.waterfall_gain_db, 0.0);
            assert_eq!(
                a.waterfall_palette,
                pancetta_config::ui::WaterfallPalette::Classic
            );
            assert!(cmd_rx.try_recv().is_err(), "{ev:?} focused={focused}");
        }
    }
}

/// PAN-389 review fix: focus left on the waterfall after a view switch
/// must not change or persist the gain or palette of a waterfall that is
/// not on screen; the keys only show the hint.
#[tokio::test]
async fn waterfall_keys_off_screen_after_a_view_switch_only_hint() {
    for view in [
        crate::view::ActiveView::Operate,
        crate::view::ActiveView::Hunt,
        crate::view::ActiveView::Run,
    ] {
        for ev in [key('{'), key('}'), key('b'), key_shift('B')] {
            let (mut r, cmd_rx, app) = make_runner().await;
            focus_waterfall(&app).await;
            app.write().await.active_view = view;
            r.handle_key_event(ev).await.unwrap();
            let a = app.read().await;
            assert_eq!(
                a.status_message(),
                "Focus the waterfall (Monitor view, Tab) to change its gain or palette",
                "{view:?} {ev:?}"
            );
            assert_eq!(a.waterfall_gain_db, 0.0, "{view:?} {ev:?}");
            assert_eq!(
                a.waterfall_palette,
                pancetta_config::ui::WaterfallPalette::Classic,
                "{view:?} {ev:?}"
            );
            assert!(cmd_rx.try_recv().is_err(), "{view:?} {ev:?}: no command");
        }
    }
}

/// PAN-389 review fix: `z` keeps the focused waterfall on screen in every
/// view (`ui::draw` renders the zoomed panel before the view dispatch),
/// so after `z` then `v` the gain and palette keys still act on it.
#[tokio::test]
async fn waterfall_keys_act_on_a_zoomed_waterfall_in_any_view() {
    for view in [
        crate::view::ActiveView::Operate,
        crate::view::ActiveView::Hunt,
        crate::view::ActiveView::Run,
    ] {
        let (mut r, cmd_rx, app) = make_runner().await;
        focus_waterfall(&app).await;
        {
            let mut a = app.write().await;
            a.zoomed = true;
            a.active_view = view;
        }
        r.handle_key_event(key('}')).await.unwrap();
        assert_eq!(app.read().await.waterfall_gain_db, 1.0, "{view:?}");
        match cmd_rx.try_recv() {
            Ok(TuiCommand::SetWaterfallGain { gain_db }) => {
                assert_eq!(gain_db, 1.0, "{view:?}")
            }
            other => panic!("{view:?}: expected SetWaterfallGain, got {other:?}"),
        }
        r.handle_key_event(key('b')).await.unwrap();
        let palette = app.read().await.waterfall_palette;
        assert_ne!(
            palette,
            pancetta_config::ui::WaterfallPalette::Classic,
            "{view:?}"
        );
        match cmd_rx.try_recv() {
            Ok(TuiCommand::SetWaterfallPalette { palette: sent }) => {
                assert_eq!(sent, palette, "{view:?}")
            }
            other => panic!("{view:?}: expected SetWaterfallPalette, got {other:?}"),
        }
    }
}

/// AC19: gain and palette are display-only; the data the TX allocator
/// and `t` key read never changes.
#[tokio::test]
async fn waterfall_gain_never_changes_waterfall_data() {
    let (mut r, _cmd_rx, app) = make_runner().await;
    focus_waterfall(&app).await;
    let rows: Vec<Vec<f32>> = (0..10)
        .map(|i| (0..32).map(|b| ((i * 7 + b) % 10) as f32 / 10.0).collect())
        .collect();
    app.write().await.waterfall_data = rows.clone();
    r.handle_key_event(key('}')).await.unwrap();
    r.handle_key_event(key('}')).await.unwrap();
    r.handle_key_event(key('b')).await.unwrap();
    r.handle_key_event(key('{')).await.unwrap();
    assert_eq!(app.read().await.waterfall_gain_db, 1.0);
    assert_eq!(app.read().await.waterfall_data, rows);
}
