//! Single source of truth for TUI keybindings.
//!
//! `KEYBINDINGS` drives the `?` help overlay
//! (`tui_runner::render_help_overlay`), the generated `docs/KEYBINDINGS.md`
//! (drift-guarded by `keybindings_doc_is_current`) AND key dispatch (PAN-327):
//! each row carries the `chords` that `resolve` matches a key event against,
//! and `TuiRunner::handle_key_event` runs `match action` on the result.
//!
//! To add or change a main-view key:
//! 1. add or edit the chord on the row whose text describes it;
//! 2. add the `Action` variant (`keymap/action.rs`, plus `ALL` and `ordinal`);
//! 3. add its `match action` arm in `TuiRunner::handle_key_event`;
//! 4. regenerate the doc:
//!    `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current`
//!
//! The table tests below then prove every chord resolves to exactly one
//! action and every action is reachable. Dialog-internal keys (y/n confirms,
//! digit entry, picker navigation) still dispatch in their own blocks in
//! `handle_key_event`.

mod action;
pub use action::Action;

use crate::app::{ActivePanel, App};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Who receives a key. `Global` is checked first in every scope.
///
/// Only the Global and Main scopes resolve through the table today; the
/// dialog scopes (out-of-band ack, quit confirm, help, pickers, modals,
/// compose and the read-only overlays) still dispatch in their own blocks in
/// `TuiRunner::handle_key_event`, ahead of the main view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Bound in every scope, ahead of every dialog (Shift+Q, PAN-113).
    Global,
    /// The main view: no dialog or overlay is open.
    Main,
}

/// Modifier policy for a chord (PAN-112). crossterm reports Ctrl+C as
/// `Char('c')` + CONTROL and Ctrl+Space as `Char(' ')` + CONTROL, and on
/// Windows Ctrl+Shift+T arrives as `Char('T')` + CONTROL|SHIFT, so every
/// TX-starting chord refuses Ctrl and Alt while still allowing Shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mods {
    /// Any modifiers.
    Any,
    /// Shift is allowed; Ctrl and Alt are refused.
    NoCtrlAlt,
    /// No modifiers at all (bare `k`, PAN-21: Ctrl+K kill-line muscle memory
    /// must not abort a QSO).
    Bare,
}

impl Mods {
    /// Whether this policy accepts the pressed modifiers.
    pub fn admits(self, m: KeyModifiers) -> bool {
        match self {
            Mods::Any => true,
            Mods::NoCtrlAlt => !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            Mods::Bare => m.is_empty(),
        }
    }
}

/// Context condition a chord needs. The conditions on chords that share a
/// key and scope are mutually exclusive, so table order never matters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    Always,
    Panel(ActivePanel),
    NotPanel(ActivePanel),
    NeitherPanel(ActivePanel, ActivePanel),
    /// The operator-stop banner is showing.
    StopBanner,
    /// Zoomed and no operator-stop banner (the banner's Esc wins).
    ZoomNoBanner,
}

impl When {
    /// Whether the condition holds in `ctx`.
    pub fn holds(self, ctx: &KeyContext) -> bool {
        match self {
            When::Always => true,
            When::Panel(p) => ctx.panel == p,
            When::NotPanel(p) => ctx.panel != p,
            When::NeitherPanel(a, b) => ctx.panel != a && ctx.panel != b,
            When::StopBanner => ctx.stopped_by_operator,
            When::ZoomNoBanner => ctx.zoomed && !ctx.stopped_by_operator,
        }
    }
}

/// One physical key, in one scope and context, bound to exactly one action.
#[derive(Debug, Clone, Copy)]
pub struct Chord {
    pub scope: Scope,
    pub code: KeyCode,
    pub mods: Mods,
    pub when: When,
    pub action: Action,
    /// Bound but deliberately absent from the row's display text (`,` `.` `_`).
    pub undocumented_alias: bool,
}

impl Chord {
    /// Whether this chord matches `key` in `scope` and `ctx`.
    pub fn matches(&self, scope: Scope, ctx: &KeyContext, key: &KeyEvent) -> bool {
        self.scope == scope
            && self.code == key.code
            && self.mods.admits(key.modifiers)
            && self.when.holds(ctx)
    }
}

const fn ch(scope: Scope, code: KeyCode, mods: Mods, when: When, action: Action) -> Chord {
    Chord {
        scope,
        code,
        mods,
        when,
        action,
        undocumented_alias: false,
    }
}

/// Main-view chord, any modifiers, no context condition.
const fn main(code: KeyCode, action: Action) -> Chord {
    ch(Scope::Main, code, Mods::Any, When::Always, action)
}

/// Main-view chord that refuses Ctrl and Alt (PAN-112).
const fn main_no_ctrl_alt(code: KeyCode, action: Action) -> Chord {
    ch(Scope::Main, code, Mods::NoCtrlAlt, When::Always, action)
}

/// Main-view chord with a context condition.
const fn main_when(code: KeyCode, mods: Mods, when: When, action: Action) -> Chord {
    ch(Scope::Main, code, mods, when, action)
}

/// Main-view chord kept out of the row's display text.
const fn main_alias(code: KeyCode, action: Action) -> Chord {
    Chord {
        undocumented_alias: true,
        ..main(code, action)
    }
}

const fn c(key: char) -> KeyCode {
    KeyCode::Char(key)
}

/// The inputs that choose a chord inside a scope.
#[derive(Debug, Clone, Copy)]
pub struct KeyContext {
    pub scope: Scope,
    pub panel: ActivePanel,
    pub zoomed: bool,
    pub stopped_by_operator: bool,
}

impl KeyContext {
    /// Main-view context from the live `App`. Only valid once no dialog has
    /// claimed the key; `handle_key_event` routes the dialogs first.
    pub fn main_view(app: &App) -> Self {
        KeyContext {
            scope: Scope::Main,
            panel: app.active_panel,
            zoomed: app.zoomed,
            stopped_by_operator: app.stopped_by_operator,
        }
    }
}

/// Every chord in the table, in row order.
pub fn all_chords() -> impl Iterator<Item = &'static Chord> {
    KEYBINDINGS.iter().flat_map(|b| b.chords.iter())
}

/// Resolve `key` in `ctx` to its action: the Global scope first, then
/// `ctx.scope`. `None` means the key does nothing there.
pub fn resolve(ctx: &KeyContext, key: KeyEvent) -> Option<Action> {
    let hit = |scope: Scope| {
        all_chords()
            .find(|chord| chord.matches(scope, ctx, &key))
            .map(|chord| chord.action)
    };
    hit(Scope::Global).or_else(|| hit(ctx.scope))
}

/// Resolve `key` in the Global scope only. `handle_key_event` calls this
/// before routing to any dialog, so no dialog can swallow Shift+Q (PAN-113).
pub fn resolve_global(key: KeyEvent) -> Option<Action> {
    let ctx = KeyContext {
        scope: Scope::Global,
        panel: ActivePanel::BandActivity,
        zoomed: false,
        stopped_by_operator: false,
    };
    resolve(&ctx, key)
}

/// Doc-grouping category for a keybinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Navigation,
    TxControl,
    QsoOperation,
    ModesViews,
    AudioDevices,
    Diagnostics,
    Session,
}

impl Category {
    /// All categories in docs/KEYBINDINGS.md section order.
    pub const ALL: [Category; 7] = [
        Category::Navigation,
        Category::QsoOperation,
        Category::TxControl,
        Category::ModesViews,
        Category::AudioDevices,
        Category::Diagnostics,
        Category::Session,
    ];

    /// Section heading used in the generated markdown.
    pub fn heading(self) -> &'static str {
        match self {
            Category::Navigation => "Navigation",
            Category::QsoOperation => "Calling & QSOs",
            Category::TxControl => "TX control",
            Category::ModesViews => "Modes & views",
            Category::AudioDevices => "Audio & devices",
            Category::Diagnostics => "Diagnostics & help",
            Category::Session => "Session & safety",
        }
    }
}

/// One keyboard binding: display key, action text, doc category, and whether
/// it appears in README.md's 10-row essentials table.
pub struct KeyBinding {
    pub key: &'static str,
    pub action: &'static str,
    pub category: Category,
    pub essential: bool,
    /// The key events this row binds (PAN-327). Display code ignores it.
    pub chords: &'static [Chord],
}

const fn kb(
    key: &'static str,
    action: &'static str,
    category: Category,
    essential: bool,
    chords: &'static [Chord],
) -> KeyBinding {
    KeyBinding {
        key,
        action,
        category,
        essential,
        chords,
    }
}

/// Every top-level TUI binding, in `?`-overlay display order, with the
/// chords key dispatch resolves through. Modal-scoped keys (y/n confirms,
/// digit entry in the freq modal, j/k in the device picker) are documented
/// by each modal's own footer, not here.
pub const KEYBINDINGS: &[KeyBinding] = &[
    kb(
        "Shift+W",
        "Watchlist: toggle focused/typed callsign (persists; removed when worked)",
        Category::QsoOperation,
        false,
        &[main_no_ctrl_alt(c('W'), Action::OpenWatchlistModal)],
    ),
    kb(
        "?",
        "Toggle this help",
        Category::Diagnostics,
        true,
        &[main(c('?'), Action::ToggleHelp)],
    ),
    kb(
        "Tab / Shift+Tab",
        "Switch panel",
        Category::Navigation,
        true,
        &[
            main(KeyCode::Tab, Action::NextPanel),
            main(KeyCode::BackTab, Action::PrevPanel),
        ],
    ),
    kb(
        "Up / Down",
        "Scroll list",
        Category::Navigation,
        true,
        &[
            main_when(
                KeyCode::Up,
                Mods::Any,
                When::Panel(ActivePanel::QsoStatus),
                Action::QsoCursorUp,
            ),
            main_when(
                KeyCode::Up,
                Mods::Any,
                When::NotPanel(ActivePanel::QsoStatus),
                Action::ListUp,
            ),
            main_when(
                KeyCode::Down,
                Mods::Any,
                When::Panel(ActivePanel::QsoStatus),
                Action::QsoCursorDown,
            ),
            main_when(
                KeyCode::Down,
                Mods::Any,
                When::NotPanel(ActivePanel::QsoStatus),
                Action::ListDown,
            ),
        ],
    ),
    kb(
        "Home / End (or < / >)",
        "Jump to newest (realtime) / oldest",
        Category::Navigation,
        false,
        &[
            main(KeyCode::Home, Action::JumpNewest),
            main(c('<'), Action::JumpNewest),
            main_alias(c(','), Action::JumpNewest),
            main(KeyCode::End, Action::JumpOldest),
            main(c('>'), Action::JumpOldest),
            main_alias(c('.'), Action::JumpOldest),
        ],
    ),
    kb(
        "PgUp / PgDn",
        "Page scroll",
        Category::Navigation,
        false,
        &[
            main(KeyCode::PageUp, Action::ListPageUp),
            main(KeyCode::PageDown, Action::ListPageDown),
        ],
    ),
    kb(
        "1/2/3/4/5",
        "Jump: Band/QSO/Callers/DX/Placement",
        Category::Navigation,
        false,
        &[
            main(c('1'), Action::FocusBandActivity),
            main(c('2'), Action::FocusQsoStatus),
            main(c('3'), Action::FocusCallers),
            main(c('4'), Action::FocusDxHunter),
            main(c('5'), Action::FocusTxPlacement),
        ],
    ),
    kb(
        "Left / Right",
        "TX offset −/+ 50 Hz (Callers: cycle reply step)",
        Category::TxControl,
        false,
        &[
            main_when(
                KeyCode::Left,
                Mods::Any,
                When::Panel(ActivePanel::Callers),
                Action::PrevReplyStep,
            ),
            main_when(
                KeyCode::Left,
                Mods::Any,
                When::Panel(ActivePanel::TxPlacement),
                Action::PlacementCursorLeft,
            ),
            main_when(
                KeyCode::Left,
                Mods::Any,
                When::NeitherPanel(ActivePanel::Callers, ActivePanel::TxPlacement),
                Action::TxOffsetDown,
            ),
            main_when(
                KeyCode::Right,
                Mods::Any,
                When::Panel(ActivePanel::Callers),
                Action::NextReplyStep,
            ),
            main_when(
                KeyCode::Right,
                Mods::Any,
                When::Panel(ActivePanel::TxPlacement),
                Action::PlacementCursorRight,
            ),
            main_when(
                KeyCode::Right,
                Mods::Any,
                When::NeitherPanel(ActivePanel::Callers, ActivePanel::TxPlacement),
                Action::TxOffsetUp,
            ),
        ],
    ),
    kb(
        "[ / ]",
        "TX offset −/+ 50 Hz",
        Category::TxControl,
        false,
        &[
            main(c('['), Action::TxOffsetDown),
            main(c(']'), Action::TxOffsetUp),
        ],
    ),
    kb(
        "= / -",
        "Band up / down",
        Category::TxControl,
        false,
        &[
            main(c('='), Action::BandUp),
            main(c('-'), Action::BandDown),
            main_alias(c('_'), Action::BandDown),
        ],
    ),
    kb(
        "Space",
        "Call selected station",
        Category::QsoOperation,
        true,
        &[main_no_ctrl_alt(c(' '), Action::CallOrReplySelected)],
    ),
    kb(
        "/",
        "Compose free-text TX (Enter sends, Esc cancels)",
        Category::QsoOperation,
        false,
        &[main(c('/'), Action::EnterCompose)],
    ),
    kb(
        "Enter",
        "Callers: reply at shown step; TX Placement: park at selected slice",
        Category::QsoOperation,
        false,
        &[
            main_when(
                KeyCode::Enter,
                Mods::NoCtrlAlt,
                When::Panel(ActivePanel::Callers),
                Action::ReplyToSelectedCaller,
            ),
            main_when(
                KeyCode::Enter,
                Mods::Any,
                When::Panel(ActivePanel::TxPlacement),
                Action::ParkAtPlacement,
            ),
            main_when(
                KeyCode::Enter,
                Mods::Any,
                When::NeitherPanel(ActivePanel::Callers, ActivePanel::TxPlacement),
                Action::ComposeHint,
            ),
        ],
    ),
    kb(
        "c / s",
        "Start / stop CQ",
        Category::QsoOperation,
        true,
        &[
            main_no_ctrl_alt(c('c'), Action::StartCq),
            main(c('s'), Action::StopCq),
        ],
    ),
    kb(
        "k",
        "Abort selected QSO now, no confirm (any panel)",
        Category::QsoOperation,
        false,
        &[main_when(
            c('k'),
            Mods::Bare,
            When::Always,
            Action::AbortSelectedQso,
        )],
    ),
    kb(
        "r",
        "Re-send last TX (QSO Status panel only)",
        Category::QsoOperation,
        false,
        &[
            main_when(
                c('r'),
                Mods::NoCtrlAlt,
                When::Panel(ActivePanel::QsoStatus),
                Action::ResendLastTx,
            ),
            main_when(
                c('r'),
                Mods::NoCtrlAlt,
                When::NotPanel(ActivePanel::QsoStatus),
                Action::ResendFocusHint,
            ),
        ],
    ),
    kb(
        "t",
        "Find clear TX offset (auto-pick + pin)",
        Category::TxControl,
        false,
        &[main(c('t'), Action::FindClearOffset)],
    ),
    kb(
        "f",
        "TX freq mode: HOLD (pin offset) / AUTO (pancetta picks)",
        Category::TxControl,
        false,
        &[main(c('f'), Action::ToggleTxFreqMode)],
    ),
    kb(
        "o",
        "Set TX audio offset Hz (blank=Auto) — implies Hold",
        Category::TxControl,
        false,
        &[main(c('o'), Action::OpenOffsetModal)],
    ),
    kb(
        "u",
        "Nudge TX offset now: active QSO or CQ (Auto mode only)",
        Category::TxControl,
        false,
        &[main_no_ctrl_alt(c('u'), Action::NudgeTxOffset)],
    ),
    kb(
        "Shift+F",
        "Set dial / split freq (RX MHz + optional TX MHz)",
        Category::TxControl,
        false,
        &[main(c('F'), Action::OpenFreqModal)],
    ),
    kb(
        "Shift+T",
        "Tune (12 s tone; blocked while TX DISABLED)",
        Category::TxControl,
        false,
        &[main_no_ctrl_alt(c('T'), Action::ToggleTune)],
    ),
    kb(
        "h",
        "Halt current TX",
        Category::TxControl,
        true,
        &[main(c('h'), Action::HaltTx)],
    ),
    kb(
        "p",
        "Toggle PTT (blocked while TX DISABLED)",
        Category::TxControl,
        false,
        &[main_no_ctrl_alt(c('p'), Action::TogglePtt)],
    ),
    kb(
        "G",
        "save gain preset for this band",
        Category::AudioDevices,
        false,
        &[main_no_ctrl_alt(c('G'), Action::SaveGainPreset)],
    ),
    kb(
        "g",
        "Cycle TX policy: Full → Respond-only → Disabled",
        Category::TxControl,
        false,
        &[main_no_ctrl_alt(c('g'), Action::CycleTxPolicy)],
    ),
    kb(
        "v / V",
        "Cycle activity view: Operate/Hunt/Run/Monitor",
        Category::ModesViews,
        false,
        &[
            main(c('v'), Action::CycleViewForward),
            main(c('V'), Action::CycleViewBack),
        ],
    ),
    kb(
        "z",
        "Zoom focused panel (again/Esc to restore)",
        Category::ModesViews,
        false,
        &[
            main(c('z'), Action::ToggleZoom),
            main_when(
                KeyCode::Esc,
                Mods::Any,
                When::ZoomNoBanner,
                Action::ClearZoom,
            ),
        ],
    ),
    kb(
        "a",
        "Toggle autonomous mode",
        Category::ModesViews,
        true,
        &[main_no_ctrl_alt(c('a'), Action::ToggleAutonomous)],
    ),
    kb(
        "Shift+P",
        "Pause / resume autonomous",
        Category::ModesViews,
        false,
        &[main(c('P'), Action::ToggleAutonomousPause)],
    ),
    kb(
        "Shift+M",
        "Cycle operating mode (FT8 → FT4; waits for coordinator confirm)",
        Category::ModesViews,
        false,
        &[main(c('M'), Action::CycleOperatingMode)],
    ),
    kb(
        "e",
        "Cycle decode-effort preset: Eco → Standard → Deep → Max → Auto",
        Category::ModesViews,
        false,
        &[main(c('e'), Action::CycleDecodeEffort)],
    ),
    kb(
        "Shift+H",
        "Engage Hound on selected DX Hunter station",
        Category::QsoOperation,
        false,
        &[
            main_when(
                c('H'),
                Mods::NoCtrlAlt,
                When::Panel(ActivePanel::DxHunter),
                Action::EngageHound,
            ),
            main_when(
                c('H'),
                Mods::NoCtrlAlt,
                When::NotPanel(ActivePanel::DxHunter),
                Action::HoundFocusHint,
            ),
        ],
    ),
    kb(
        "Shift+X",
        "Toggle Fox (DXpedition) mode",
        Category::ModesViews,
        false,
        &[main_no_ctrl_alt(c('X'), Action::ToggleFoxMode)],
    ),
    kb(
        "Shift+D",
        "Toggle Diagnostics overlay (retained event history)",
        Category::Diagnostics,
        false,
        &[main(c('D'), Action::OpenDiagnostics)],
    ),
    kb(
        "Shift+S",
        "Toggle station-health panel (is the station healthy?)",
        Category::Diagnostics,
        false,
        &[main(c('S'), Action::OpenHealth)],
    ),
    kb(
        "!",
        "Acknowledge yellow readiness warnings (red blockers stay until resolved)",
        Category::Diagnostics,
        false,
        &[main(c('!'), Action::AckReadiness)],
    ),
    kb(
        "Shift+R",
        "Toggle Recent-QSOs panel (retained terminal-QSO outcome history)",
        Category::Diagnostics,
        false,
        &[main(c('R'), Action::OpenRecentQsos)],
    ),
    kb(
        "w",
        "Why this station? Reason and score breakdown for the focused station",
        Category::Diagnostics,
        false,
        &[main_no_ctrl_alt(c('w'), Action::OpenWhy)],
    ),
    kb(
        "m",
        "Toggle audio monitoring",
        Category::AudioDevices,
        false,
        &[main(c('m'), Action::ToggleMonitoring)],
    ),
    kb(
        "d",
        "Device picker",
        Category::AudioDevices,
        true,
        &[main(c('d'), Action::OpenDevicePicker)],
    ),
    kb(
        "i",
        "Rig config picker (model/port/baud/PTT)",
        Category::AudioDevices,
        false,
        &[main(c('i'), Action::OpenRigPicker)],
    ),
    kb(
        "x",
        "Clear decoded messages (press twice within 3s)",
        Category::Diagnostics,
        false,
        &[main(c('x'), Action::ClearDecodes)],
    ),
    kb(
        "q",
        "Quit (with confirm)",
        Category::Session,
        true,
        &[main(c('q'), Action::OpenQuitConfirm)],
    ),
    kb(
        "Shift+Q",
        "EMERGENCY STOP (halt TX, autonomous off)",
        Category::Session,
        true,
        &[ch(
            Scope::Global,
            c('Q'),
            Mods::Any,
            When::Always,
            Action::EmergencyStop,
        )],
    ),
    kb(
        "Esc",
        "Dismiss overlay / cancel modal / clear stop banner",
        Category::Session,
        false,
        &[main_when(
            KeyCode::Esc,
            Mods::Any,
            When::StopBanner,
            Action::ClearStopBanner,
        )],
    ),
];

/// Render the generated `docs/KEYBINDINGS.md` content.
pub fn render_markdown() -> String {
    let mut out = String::from(
        "<!-- GENERATED FILE - do not edit by hand.\n     \
         Source: pancetta-tui/src/keymap.rs (KEYBINDINGS).\n     \
         Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current -->\n\n\
         # Pancetta TUI keybindings\n\n\
         Press `?` inside the TUI for the same list as an overlay.\n",
    );
    for cat in Category::ALL {
        out.push_str(&format!(
            "\n## {}\n\n| Key | Action |\n|---|---|\n",
            cat.heading()
        ));
        for b in KEYBINDINGS.iter().filter(|b| b.category == cat) {
            out.push_str(&format!("| `{}` | {} |\n", b.key, b.action));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard, same philosophy as pancetta-config's merge_with guard
    /// (pancetta-config/src/lib.rs merge_guard): the generated doc failing to
    /// match the table fails a test, not a human review.
    #[test]
    fn keybindings_doc_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/KEYBINDINGS.md");
        let expected = render_markdown();
        if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
            std::fs::write(path, &expected).expect("write docs/KEYBINDINGS.md");
            return;
        }
        let actual = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            actual, expected,
            "docs/KEYBINDINGS.md is stale. Regenerate with:\n  \
             PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current"
        );
    }

    #[test]
    fn essentials_table_is_exactly_ten_rows() {
        let n = KEYBINDINGS.iter().filter(|b| b.essential).count();
        assert_eq!(
            n, 10,
            "README essentials table is specified as exactly 10 rows"
        );
    }

    // ---- PAN-327: key dispatch through the table -------------------------

    const PANELS: [ActivePanel; 5] = [
        ActivePanel::BandActivity,
        ActivePanel::QsoStatus,
        ActivePanel::Callers,
        ActivePanel::DxHunter,
        ActivePanel::TxPlacement,
    ];

    const MOD_SETS: [KeyModifiers; 5] = [
        KeyModifiers::NONE,
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
    ];

    /// Every context sample: 5 panels x zoomed x stopped.
    fn contexts(scope: Scope) -> Vec<KeyContext> {
        let mut out = Vec::new();
        for panel in PANELS {
            for zoomed in [false, true] {
                for stopped_by_operator in [false, true] {
                    out.push(KeyContext {
                        scope,
                        panel,
                        zoomed,
                        stopped_by_operator,
                    });
                }
            }
        }
        out
    }

    /// A context in `scope` where `when` holds.
    fn context_for(scope: Scope, when: When) -> KeyContext {
        contexts(scope)
            .into_iter()
            .find(|ctx| when.holds(ctx))
            .unwrap_or_else(|| panic!("no context satisfies {when:?}"))
    }

    /// Every key code any chord binds, plus all printable ASCII.
    fn key_universe() -> Vec<KeyCode> {
        let mut codes: Vec<KeyCode> = (' '..='~').map(KeyCode::Char).collect();
        for chord in all_chords() {
            if !codes.contains(&chord.code) {
                codes.push(chord.code);
            }
        }
        codes
    }

    #[test]
    fn every_chord_resolves_to_its_own_action() {
        for chord in all_chords() {
            let ctx = context_for(chord.scope, chord.when);
            let got = resolve(&ctx, KeyEvent::new(chord.code, KeyModifiers::NONE));
            assert_eq!(
                got,
                Some(chord.action),
                "{:?} in {:?} ({:?}) resolved to {got:?}",
                chord.code,
                chord.scope,
                chord.when
            );
        }
    }

    /// "Every table entry maps to exactly one action", made
    /// order-independent: in any context, at most one chord matches a key.
    #[test]
    fn no_two_chords_match_the_same_key_in_the_same_context() {
        for scope in [Scope::Global, Scope::Main] {
            for ctx in contexts(scope) {
                for code in key_universe() {
                    for mods in MOD_SETS {
                        let key = KeyEvent::new(code, mods);
                        let hits: Vec<&Chord> = all_chords()
                            .filter(|c| {
                                c.matches(Scope::Global, &ctx, &key) || c.matches(scope, &ctx, &key)
                            })
                            .collect();
                        assert!(
                            hits.len() <= 1,
                            "{code:?} {mods:?} in {ctx:?} matches {} chords: {:?}",
                            hits.len(),
                            hits.iter().map(|c| c.action).collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_action_is_reachable() {
        for action in Action::ALL {
            assert!(
                all_chords().any(|c| c.action == action),
                "{action:?} has no chord, so no key can reach it"
            );
        }
    }

    #[test]
    fn all_lists_every_variant_once() {
        for (i, action) in Action::ALL.iter().enumerate() {
            assert_eq!(action.ordinal(), i, "{action:?} is out of place in ALL");
        }
    }

    #[test]
    fn every_keybindings_row_has_at_least_one_chord() {
        for row in KEYBINDINGS {
            assert!(!row.chords.is_empty(), "row {:?} binds no chord", row.key);
        }
    }

    /// Display labels a chord's key may appear under in a row's key text.
    fn labels(code: KeyCode) -> Vec<String> {
        match code {
            KeyCode::Char(' ') => vec!["Space".into()],
            KeyCode::Char(c) if c.is_ascii_uppercase() => vec![format!("Shift+{c}"), c.into()],
            KeyCode::Char(c) => vec![c.into()],
            KeyCode::BackTab => vec!["Shift+Tab".into()],
            KeyCode::PageUp => vec!["PgUp".into()],
            KeyCode::PageDown => vec!["PgDn".into()],
            other => vec![format!("{other:?}")],
        }
    }

    /// Key tokens of a row's display text: "Home / End (or < / >)" yields
    /// Home, End, <, >; "1/2/3/4/5" yields 1..5; "/" stays "/".
    fn key_tokens(key: &str) -> Vec<String> {
        if key == "/" {
            return vec!["/".into()];
        }
        let mut out = Vec::new();
        for token in key.split_whitespace() {
            if token == "/" {
                continue;
            }
            for part in token.split('/') {
                let part = part.trim_matches(|c| c == '(' || c == ')');
                if !part.is_empty() && part != "or" {
                    out.push(part.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn every_chord_is_named_by_its_row() {
        for row in KEYBINDINGS {
            let tokens = key_tokens(row.key);
            for chord in row.chords.iter().filter(|c| !c.undocumented_alias) {
                let named = labels(chord.code).iter().any(|label| {
                    tokens.contains(label)
                        || ((label == "Enter" || label == "Esc")
                            && row.action.contains(label.as_str()))
                });
                assert!(
                    named,
                    "row {:?} binds {:?}, which its text does not name",
                    row.key, chord.code
                );
            }
        }
    }

    #[test]
    fn undocumented_aliases_are_exactly_comma_period_underscore() {
        let mut aliases: Vec<KeyCode> = all_chords()
            .filter(|c| c.undocumented_alias)
            .map(|c| c.code)
            .collect();
        aliases.sort_by_key(|code| format!("{code:?}"));
        assert_eq!(
            aliases,
            vec![KeyCode::Char(','), KeyCode::Char('.'), KeyCode::Char('_')]
        );
    }

    /// PAN-112 as a table rule: no Ctrl- or Alt-reachable TX key.
    #[test]
    fn tx_starting_main_chords_refuse_ctrl_and_alt() {
        for chord in all_chords().filter(|c| c.scope == Scope::Main && c.action.starts_tx()) {
            assert!(
                matches!(chord.mods, Mods::NoCtrlAlt | Mods::Bare),
                "{:?} on {:?} starts TX but admits Ctrl/Alt",
                chord.action,
                chord.code
            );
        }
    }

    /// PAN-113: Shift+Q wins in every scope, with any modifiers.
    #[test]
    fn shift_q_resolves_to_emergency_stop_in_every_scope() {
        for scope in [Scope::Global, Scope::Main] {
            for ctx in contexts(scope) {
                for mods in [
                    KeyModifiers::NONE,
                    KeyModifiers::SHIFT,
                    KeyModifiers::CONTROL,
                    KeyModifiers::ALT,
                ] {
                    let key = KeyEvent::new(KeyCode::Char('Q'), mods);
                    assert_eq!(resolve(&ctx, key), Some(Action::EmergencyStop));
                    assert_eq!(resolve_global(key), Some(Action::EmergencyStop));
                }
            }
        }
    }

    /// Only Shift+Q resolves in the Global scope, so `resolve_global` can
    /// never steal a key from an open dialog.
    #[test]
    fn global_scope_binds_only_shift_q() {
        let global: Vec<Action> = all_chords()
            .filter(|c| c.scope == Scope::Global)
            .map(|c| c.action)
            .collect();
        assert_eq!(global, vec![Action::EmergencyStop]);
    }

    #[test]
    fn keybinding_display_fields_are_unchanged() {
        // The drift test above guards the rendered text; this guards the
        // row count the help overlay and README essentials depend on.
        assert_eq!(KEYBINDINGS.len(), 46);
    }

    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for b in KEYBINDINGS {
            assert!(seen.insert(b.key), "duplicate keybinding entry: {}", b.key);
        }
    }
}
