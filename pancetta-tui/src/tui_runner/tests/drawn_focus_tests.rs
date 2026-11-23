// PAN-419. Focus stays on a panel the active view draws: a view switch
// (`v`/`V`), a jump key (`1`-`5`), clearing zoom and startup in a saved view
// never leave `active_panel` on a panel the screen does not show.
use super::key_tests::{key, key_shift, make_runner};
use super::*;
use crate::app::ActivePanel;
use crate::view::ActiveView;

const VIEWS: [ActiveView; 4] = [
    ActiveView::Operate,
    ActiveView::Hunt,
    ActiveView::Run,
    ActiveView::Monitor,
];

/// The jump keys and the panel each one names, in key order.
const JUMPS: [(char, ActivePanel, &str); 5] = [
    ('1', ActivePanel::BandActivity, "Band Activity"),
    ('2', ActivePanel::QsoStatus, "QSO Status"),
    ('3', ActivePanel::Callers, "Callers"),
    ('4', ActivePanel::DxHunter, "DX Hunter"),
    ('5', ActivePanel::TxPlacement, "TX Placement"),
];

/// Put a fresh runner's app in `view`, focused on `panel`, unzoomed.
async fn focus(app: &Arc<RwLock<App>>, view: ActiveView, panel: ActivePanel) {
    let mut a = app.write().await;
    a.active_view = view;
    a.active_panel = panel;
    a.zoomed = false;
}

/// The bottom row (the footer) of `app` drawn at 120x40.
fn footer_row(app: &App) -> String {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    term.draw(|f| crate::ui::draw(f, app).unwrap()).unwrap();
    let buf = term.backend().buffer();
    let y = buf.area.height - 1;
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

/// AC1, ticket scenario 1: Operate with QSO Status focused, then `v` until
/// Monitor. Monitor does not draw QSO Status, so focus lands on its first
/// panel, the waterfall, and the footer shows the waterfall's keys.
#[tokio::test]
async fn v_from_operate_qso_status_to_monitor_focuses_a_drawn_panel() {
    let (mut r, _cmd_rx, app) = make_runner().await;
    focus(&app, ActiveView::Operate, ActivePanel::QsoStatus).await;
    for _ in 0..3 {
        r.handle_key_event(key('v')).await.unwrap();
    }
    let a = app.read().await;
    assert_eq!(a.active_view, ActiveView::Monitor);
    assert_eq!(a.active_panel, ActivePanel::Waterfall);
    let footer = footer_row(&a);
    assert!(footer.contains("{/}:Gain"), "footer: {footer}");
    assert!(!footer.contains("r:Resend"), "footer: {footer}");
    assert!(!footer.contains("k:Abort"), "footer: {footer}");
}

/// AC2, ticket scenario 2: `4` in Monitor does not focus DX Hunter, which
/// Monitor does not draw; the status line says why and nothing is sent.
#[tokio::test]
async fn jump_4_in_monitor_does_not_focus_dx_hunter() {
    let (mut r, cmd_rx, app) = make_runner().await;
    focus(&app, ActiveView::Monitor, ActivePanel::Waterfall).await;
    r.handle_key_event(key('4')).await.unwrap();
    let a = app.read().await;
    assert_eq!(a.active_panel, ActivePanel::Waterfall);
    assert_eq!(a.status_message(), "DX Hunter is not shown in Monitor view");
    assert!(cmd_rx.try_recv().is_err(), "a refused jump sends nothing");
}

/// AC3: every (view, jump key) pair, from the view's first panel. A drawn
/// panel takes focus with `Panel: <name>`; an undrawn one leaves focus put
/// and names the panel and the view.
#[tokio::test]
async fn jump_keys_focus_only_drawn_panels_in_every_view() {
    for view in VIEWS {
        for (c, panel, name) in JUMPS {
            let (mut r, cmd_rx, app) = make_runner().await;
            let start = ActivePanel::tab_order(view)[0];
            focus(&app, view, start).await;
            r.handle_key_event(key(c)).await.unwrap();
            let a = app.read().await;
            if ActivePanel::tab_order(view).contains(&panel) {
                assert_eq!(a.active_panel, panel, "{view:?} {c}");
                assert_eq!(a.status_message(), format!("Panel: {name}"), "{view:?} {c}");
            } else {
                assert_eq!(a.active_panel, start, "{view:?} {c}");
                assert_eq!(
                    a.status_message(),
                    format!(
                        "{name} is not shown in {}",
                        crate::presentation::view_message(view)
                    ),
                    "{view:?} {c}"
                );
            }
            assert!(cmd_rx.try_recv().is_err(), "{view:?} {c}: no command");
        }
    }
}

/// AC4: from every drawn panel of every view, each of four `v` presses,
/// and separately four `V` presses, leaves focus on a panel the new view
/// draws.
#[tokio::test]
async fn view_cycle_keeps_focus_on_a_drawn_panel_from_every_start() {
    for ev in [key('v'), key_shift('V')] {
        for view in VIEWS {
            for &panel in ActivePanel::tab_order(view) {
                let (mut r, _cmd_rx, app) = make_runner().await;
                focus(&app, view, panel).await;
                for press in 1..=4 {
                    r.handle_key_event(ev).await.unwrap();
                    let a = app.read().await;
                    assert!(
                        ActivePanel::tab_order(a.active_view).contains(&a.active_panel),
                        "{ev:?} from {view:?}/{panel:?}, press {press}: \
                         {:?} focused in {:?}",
                        a.active_panel,
                        a.active_view
                    );
                }
            }
        }
    }
}

/// AC5: a focus the new view still draws is kept. Operate, Hunt and Run
/// all draw QSO Status.
#[tokio::test]
async fn view_cycle_keeps_a_focus_the_new_view_still_draws() {
    let (mut r, _cmd_rx, app) = make_runner().await;
    focus(&app, ActiveView::Operate, ActivePanel::QsoStatus).await;
    for expected in [ActiveView::Hunt, ActiveView::Run] {
        r.handle_key_event(key('v')).await.unwrap();
        let a = app.read().await;
        assert_eq!(a.active_view, expected);
        assert_eq!(a.active_panel, ActivePanel::QsoStatus, "{expected:?}");
    }
}

/// AC6: zoomed, the focused panel fills the screen in any view, so `v`
/// keeps focus and a jump key focuses its panel whatever the view. `z`
/// clearing the zoom moves an undrawn focus to the view's first panel.
#[tokio::test]
async fn zoom_keeps_focus_until_it_clears() {
    let (mut r, cmd_rx, app) = make_runner().await;
    focus(&app, ActiveView::Operate, ActivePanel::QsoStatus).await;
    r.handle_key_event(key('z')).await.unwrap();
    assert!(app.read().await.zoomed);
    for _ in 0..3 {
        r.handle_key_event(key('v')).await.unwrap();
    }
    {
        let a = app.read().await;
        assert_eq!(a.active_view, ActiveView::Monitor);
        assert_eq!(a.active_panel, ActivePanel::QsoStatus, "zoom keeps focus");
        assert!(a.zoomed);
    }
    r.handle_key_event(key('4')).await.unwrap();
    {
        let a = app.read().await;
        assert_eq!(a.active_panel, ActivePanel::DxHunter, "zoomed jump focuses");
        assert_eq!(a.status_message(), "Panel: DX Hunter");
    }
    r.handle_key_event(key('z')).await.unwrap();
    let a = app.read().await;
    assert!(!a.zoomed);
    assert_eq!(
        a.active_panel,
        ActivePanel::tab_order(ActiveView::Monitor)[0]
    );
    assert!(cmd_rx.try_recv().is_err(), "zoom and focus are local-only");
}

/// AC6: Esc clearing the zoom also moves an undrawn focus onto the view's
/// first panel.
#[tokio::test]
async fn esc_clearing_zoom_moves_focus_onto_a_drawn_panel() {
    let (mut r, _cmd_rx, app) = make_runner().await;
    {
        let mut a = app.write().await;
        a.active_view = ActiveView::Run;
        a.active_panel = ActivePanel::DxHunter;
        a.zoomed = true;
    }
    r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    let a = app.read().await;
    assert!(!a.zoomed);
    assert_eq!(a.status_message(), "Zoom cleared");
    assert_eq!(a.active_panel, ActivePanel::tab_order(ActiveView::Run)[0]);
}

/// AC7: starting in a saved view focuses a panel that view draws. Run does
/// not draw Band Activity, so it starts on TX Placement; the other views
/// draw Band Activity and keep it.
#[tokio::test]
async fn startup_in_a_persisted_view_focuses_a_panel_it_draws() {
    let dir = tempfile::tempdir().unwrap();
    for (view, expected) in [
        (ActiveView::Operate, ActivePanel::BandActivity),
        (ActiveView::Hunt, ActivePanel::BandActivity),
        (ActiveView::Run, ActivePanel::TxPlacement),
        (ActiveView::Monitor, ActivePanel::BandActivity),
    ] {
        let path = dir.path().join(format!("tui_state_{}.json", view.as_str()));
        std::fs::write(&path, format!("{{\"active_view\":\"{}\"}}", view.as_str())).unwrap();
        App::set_test_tui_state_path(path);
        let app = App::new(Config::default(), None).await.unwrap();
        assert_eq!(app.active_view, view, "persisted {view:?}");
        assert_eq!(app.active_panel, expected, "persisted {view:?}");
    }
}
