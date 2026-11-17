// PAN-388: the footer hint table matches the keymap and keeps Help and Quit last.
use super::*;
use crate::keymap::{resolve, KeyContext, Scope};
use crossterm::event::KeyEvent;

const PANELS: [ActivePanel; 6] = [
    ActivePanel::BandActivity,
    ActivePanel::QsoStatus,
    ActivePanel::Callers,
    ActivePanel::DxHunter,
    ActivePanel::TxPlacement,
    ActivePanel::Waterfall,
];

#[test]
fn every_footer_hint_resolves_to_its_action() {
    for panel in PANELS {
        for zoomed in [false, true] {
            for stopped_by_operator in [false, true] {
                let ctx = KeyContext {
                    scope: Scope::Main,
                    panel,
                    zoomed,
                    stopped_by_operator,
                    rig_field_is_model: false,
                };
                for h in footer_hints(panel) {
                    assert!(!h.binds.is_empty(), "{panel:?} hint {} binds no key", h.key);
                    for &(code, mods, action) in h.binds {
                        assert_eq!(
                            resolve(&ctx, KeyEvent::new(code, mods)),
                            Some(action),
                            "{panel:?} hint {}{} key {code:?}+{mods:?} \
                             (zoomed={zoomed}, stop={stopped_by_operator})",
                            h.key,
                            h.label,
                        );
                    }
                }
            }
        }
    }
}

/// The texts a footer may use for `code`.
fn key_names(code: KeyCode) -> Vec<String> {
    match code {
        KeyCode::Char(' ') => vec!["Space".to_string()],
        KeyCode::Char(c) if c.is_ascii_uppercase() => vec![c.to_string(), format!("Shift+{c}")],
        KeyCode::Char(c) => vec![c.to_string()],
        other => vec![format!("{other:?}")],
    }
}

#[test]
fn footer_hint_key_text_names_its_keys() {
    for panel in PANELS {
        for h in footer_hints(panel) {
            // Band Activity's legacy hint: `Arrows` stands for Left/Right.
            if h.key == "Arrows" {
                continue;
            }
            let tokens: Vec<&str> = h.key.split('/').collect();
            for &(code, _, _) in h.binds {
                let names = key_names(code);
                assert!(
                    names.iter().any(|n| tokens.contains(&n.as_str())),
                    "{panel:?} hint {:?} does not name key {code:?} (tokens {tokens:?})",
                    h.key,
                );
            }
        }
    }
}

#[test]
fn dx_hunter_hints_name_the_ticket_actions() {
    let actions: Vec<Action> = panel_hints(ActivePanel::DxHunter)
        .iter()
        .flat_map(|h| h.binds.iter().map(|&(_, _, a)| a))
        .collect();
    assert_eq!(
        actions,
        vec![
            Action::CallOrReplySelected,
            Action::OpenWhy,
            Action::OpenWatchlistModal,
            Action::LookupSelectedStation,
            Action::EngageHound,
        ]
    );
    let band_keys: Vec<&str> = panel_hints(ActivePanel::BandActivity)
        .iter()
        .map(|h| h.key)
        .collect();
    for h in panel_hints(ActivePanel::DxHunter) {
        if h.key != "Space" {
            assert!(
                !band_keys.contains(&h.key),
                "DX Hunter hint {} repeats a Band Activity key",
                h.key
            );
        }
    }
}

#[test]
fn help_and_quit_are_pinned_last() {
    for panel in PANELS {
        let all: Vec<&FooterHint> = footer_hints(panel).collect();
        let n = all.len();
        assert!(n >= 2);
        assert_eq!(all[n - 2].key, "?");
        assert_eq!(all[n - 2].binds[0].2, Action::ToggleHelp);
        assert_eq!(all[n - 2].admit, 0);
        assert_eq!(all[n - 1].key, "q");
        assert_eq!(all[n - 1].binds[0].2, Action::OpenQuitConfirm);
        assert_eq!(all[n - 1].admit, 0);
        for h in panel_hints(panel) {
            assert_ne!(h.admit, 0, "{panel:?} hint {} must not be pinned", h.key);
        }
    }
}

#[test]
fn waterfall_hints_name_its_gain_and_palette_keys() {
    let actions: Vec<Action> = panel_hints(ActivePanel::Waterfall)
        .iter()
        .flat_map(|h| h.binds.iter().map(|&(_, _, a)| a))
        .collect();
    assert_eq!(
        actions,
        vec![
            Action::WaterfallGainDown,
            Action::WaterfallGainUp,
            Action::CycleWaterfallPaletteForward,
            Action::CycleWaterfallPaletteBack,
        ]
    );
}
