// PAN-388: Enter resolves to each main-view panel's own action.
use super::*;

/// Enter on DX Hunter opens the Why details overlay.
#[test]
fn enter_resolves_per_panel() {
    for (panel, expected) in [
        (ActivePanel::DxHunter, Action::OpenWhy),
        (ActivePanel::BandActivity, Action::ComposeHint),
        (ActivePanel::QsoStatus, Action::ComposeHint),
        (ActivePanel::Callers, Action::ReplyToSelectedCaller),
        (ActivePanel::TxPlacement, Action::ParkAtPlacement),
        (ActivePanel::Waterfall, Action::ComposeHint),
    ] {
        let ctx = KeyContext {
            scope: Scope::Main,
            panel,
            zoomed: false,
            stopped_by_operator: false,
            rig_field_is_model: false,
        };
        assert_eq!(
            resolve(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(expected),
            "Enter on {panel:?}"
        );
    }
}
