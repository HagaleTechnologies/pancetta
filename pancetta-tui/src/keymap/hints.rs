//! Footer key hints per focused panel (PAN-388).
//!
//! `ui::footer_line` renders the active view's name, `panel_hints(panel)`,
//! then `TRAILER`, through `bar_fit`. Every hint lists the keys it names and
//! the Main-scope `Action` each must resolve to;
//! `every_footer_hint_resolves_to_its_action` checks them against
//! `keymap::resolve`, so a hint cannot advertise a key that does something else.
use super::Action;
use crate::app::ActivePanel;
use crossterm::event::{KeyCode, KeyModifiers};

/// One key a hint names and the action it must resolve to in the main view.
pub(crate) type HintBind = (KeyCode, KeyModifiers, Action);

/// One `key:label` segment of the footer hint row.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FooterHint {
    /// Key text, drawn bold in the theme's `key` token (`Space`, `Enter`, `W`, `=/-`).
    pub key: &'static str,
    /// Label after the key, starting with `:`.
    pub label: &'static str,
    /// Shorter label used until the full one fits.
    pub compact_label: Option<&'static str>,
    /// `bar_fit` admit rank: lower is more important; 0 is pinned.
    pub admit: u8,
    /// `bar_fit` upgrade rank (compact to full); ignored without `compact_label`.
    pub upgrade: u8,
    /// The keys this hint names, each with the action it must resolve to.
    pub binds: &'static [HintBind],
}

/// Upgrade rank of the view name (`Operate` to `Operate view`).
pub(crate) const VIEW_UPGRADE_RANK: u8 = 7;

const NONE: KeyModifiers = KeyModifiers::NONE;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

const fn hint(
    key: &'static str,
    label: &'static str,
    admit: u8,
    binds: &'static [HintBind],
) -> FooterHint {
    FooterHint {
        key,
        label,
        compact_label: None,
        admit,
        upgrade: 0,
        binds,
    }
}

const BAND_ACTIVITY: &[FooterHint] = &[
    FooterHint {
        key: "Space",
        label: ":Call/Reply",
        compact_label: Some(":Call"),
        admit: 1,
        upgrade: 8,
        binds: &[(KeyCode::Char(' '), NONE, Action::CallOrReplySelected)],
    },
    // Legacy hint text: Left/Right nudge the TX offset here.
    hint(
        "Arrows",
        ":TX",
        3,
        &[
            (KeyCode::Left, NONE, Action::TxOffsetDown),
            (KeyCode::Right, NONE, Action::TxOffsetUp),
        ],
    ),
    hint(
        "=/-",
        ":Band",
        5,
        &[
            (KeyCode::Char('='), NONE, Action::BandUp),
            (KeyCode::Char('-'), NONE, Action::BandDown),
        ],
    ),
    hint(
        "g",
        ":TX-policy",
        2,
        &[(KeyCode::Char('g'), NONE, Action::CycleTxPolicy)],
    ),
    hint(
        "d",
        ":Audio",
        4,
        &[(KeyCode::Char('d'), NONE, Action::OpenDevicePicker)],
    ),
];

const DX_HUNTER: &[FooterHint] = &[
    hint(
        "Space",
        ":Call",
        1,
        &[(KeyCode::Char(' '), NONE, Action::CallOrReplySelected)],
    ),
    hint(
        "Enter",
        ":Details",
        2,
        &[(KeyCode::Enter, NONE, Action::OpenWhy)],
    ),
    hint(
        "W",
        ":Watchlist",
        3,
        &[(KeyCode::Char('W'), SHIFT, Action::OpenWatchlistModal)],
    ),
    hint(
        "L",
        ":Lookup",
        4,
        &[(KeyCode::Char('L'), SHIFT, Action::LookupSelectedStation)],
    ),
    hint(
        "H",
        ":Hound",
        6,
        &[(KeyCode::Char('H'), SHIFT, Action::EngageHound)],
    ),
];

const CALLERS: &[FooterHint] = &[
    hint(
        "Enter",
        ":Reply",
        1,
        &[(KeyCode::Enter, NONE, Action::ReplyToSelectedCaller)],
    ),
    hint(
        "Left/Right",
        ":Step",
        2,
        &[
            (KeyCode::Left, NONE, Action::PrevReplyStep),
            (KeyCode::Right, NONE, Action::NextReplyStep),
        ],
    ),
    hint(
        "Space",
        ":Call/Reply",
        3,
        &[(KeyCode::Char(' '), NONE, Action::CallOrReplySelected)],
    ),
];

const QSO_STATUS: &[FooterHint] = &[
    hint(
        "Up/Down",
        ":Select",
        1,
        &[
            (KeyCode::Up, NONE, Action::QsoCursorUp),
            (KeyCode::Down, NONE, Action::QsoCursorDown),
        ],
    ),
    hint(
        "r",
        ":Resend",
        2,
        &[(KeyCode::Char('r'), NONE, Action::ResendLastTx)],
    ),
    hint(
        "k",
        ":Abort",
        3,
        &[(KeyCode::Char('k'), NONE, Action::AbortSelectedQso)],
    ),
];

/// No Space hint: TX Placement has no focused station for Space to act on.
const TX_PLACEMENT: &[FooterHint] = &[
    hint(
        "Left/Right",
        ":Pick",
        1,
        &[
            (KeyCode::Left, NONE, Action::PlacementCursorLeft),
            (KeyCode::Right, NONE, Action::PlacementCursorRight),
        ],
    ),
    hint(
        "Enter",
        ":Park",
        2,
        &[(KeyCode::Enter, NONE, Action::ParkAtPlacement)],
    ),
];

/// Monitor-view waterfall (PAN-389): its gain and palette keys work only
/// while it is focused.
const WATERFALL: &[FooterHint] = &[
    hint(
        "{/}",
        ":Gain",
        1,
        &[
            (KeyCode::Char('{'), NONE, Action::WaterfallGainDown),
            (KeyCode::Char('}'), NONE, Action::WaterfallGainUp),
        ],
    ),
    hint(
        "b/B",
        ":Palette",
        2,
        &[
            (
                KeyCode::Char('b'),
                NONE,
                Action::CycleWaterfallPaletteForward,
            ),
            (KeyCode::Char('B'), SHIFT, Action::CycleWaterfallPaletteBack),
        ],
    ),
];

/// Hints after every panel's set: how to change view, then the pinned pair.
const TRAILER: &[FooterHint] = &[
    hint(
        "v",
        ":View",
        9,
        &[(KeyCode::Char('v'), NONE, Action::CycleViewForward)],
    ),
    hint(
        "?",
        ":Help",
        0,
        &[(KeyCode::Char('?'), NONE, Action::ToggleHelp)],
    ),
    hint(
        "q",
        ":Quit",
        0,
        &[(KeyCode::Char('q'), NONE, Action::OpenQuitConfirm)],
    ),
];

/// The focused panel's own hints, in display order.
pub(crate) fn panel_hints(panel: ActivePanel) -> &'static [FooterHint] {
    match panel {
        ActivePanel::BandActivity => BAND_ACTIVITY,
        ActivePanel::QsoStatus => QSO_STATUS,
        ActivePanel::Callers => CALLERS,
        ActivePanel::DxHunter => DX_HUNTER,
        ActivePanel::TxPlacement => TX_PLACEMENT,
        ActivePanel::Waterfall => WATERFALL,
    }
}

/// Every hint the footer shows for `panel`: its own, then `TRAILER`.
pub(crate) fn footer_hints(panel: ActivePanel) -> impl Iterator<Item = &'static FooterHint> {
    panel_hints(panel).iter().chain(TRAILER.iter())
}

#[cfg(test)]
#[path = "hints/tests/footer_hint_tests.rs"]
mod footer_hint_tests;
