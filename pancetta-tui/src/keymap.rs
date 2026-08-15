//! Single source of truth for TUI keybindings.
//!
//! `KEYBINDINGS` drives BOTH the `?` help overlay
//! (`tui_runner::render_help_overlay`) and the generated
//! `docs/KEYBINDINGS.md` (drift-guarded by `keybindings_doc_is_current`).
//! When you add or change a key handler in `tui_runner.rs`, update this
//! table, then regenerate the doc:
//! `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current`

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
}

const fn kb(
    key: &'static str,
    action: &'static str,
    category: Category,
    essential: bool,
) -> KeyBinding {
    KeyBinding {
        key,
        action,
        category,
        essential,
    }
}

/// Every top-level TUI binding, in `?`-overlay display order.
/// Modal-scoped keys (y/n confirms, digit entry in the freq modal, j/k in
/// the device picker) are documented by each modal's own footer, not here.
pub const KEYBINDINGS: &[KeyBinding] = &[
    kb("?", "Toggle this help", Category::Diagnostics, true),
    kb(
        "Tab / Shift+Tab",
        "Switch panel",
        Category::Navigation,
        true,
    ),
    kb("Up / Down", "Scroll list", Category::Navigation, true),
    kb(
        "Home / End (or < / >)",
        "Jump to newest (realtime) / oldest",
        Category::Navigation,
        false,
    ),
    kb("PgUp / PgDn", "Page scroll", Category::Navigation, false),
    kb(
        "1/2/3/4/5",
        "Jump: Band/QSO/Callers/DX/Placement",
        Category::Navigation,
        false,
    ),
    kb(
        "Left / Right",
        "TX offset −/+ 50 Hz (Callers: cycle reply step)",
        Category::TxControl,
        false,
    ),
    kb("[ / ]", "TX offset −/+ 50 Hz", Category::TxControl, false),
    kb("= / -", "Band up / down", Category::TxControl, false),
    kb(
        "Space",
        "Call selected station",
        Category::QsoOperation,
        true,
    ),
    kb(
        "/",
        "Compose free-text TX (Enter sends, Esc cancels)",
        Category::QsoOperation,
        false,
    ),
    kb(
        "Enter",
        "Callers: reply at shown step; TX Placement: park at selected slice",
        Category::QsoOperation,
        false,
    ),
    kb("c / s", "Start / stop CQ", Category::QsoOperation, true),
    kb(
        "k",
        "Abort selected QSO (any panel)",
        Category::QsoOperation,
        false,
    ),
    kb(
        "r",
        "Re-send last TX (QSO Status panel only)",
        Category::QsoOperation,
        false,
    ),
    kb(
        "t",
        "Find clear TX offset (auto-pick + pin)",
        Category::TxControl,
        false,
    ),
    kb(
        "f",
        "TX freq mode: HOLD (pin offset) / AUTO (pancetta picks)",
        Category::TxControl,
        false,
    ),
    kb(
        "o",
        "Set TX audio offset Hz (blank=Auto) — implies Hold",
        Category::TxControl,
        false,
    ),
    kb(
        "Shift+F",
        "Set dial / split freq (RX MHz + optional TX MHz)",
        Category::TxControl,
        false,
    ),
    kb(
        "Shift+T",
        "Tune (12 s tone; blocked while TX DISABLED)",
        Category::TxControl,
        false,
    ),
    kb("h", "Halt current TX", Category::TxControl, true),
    kb(
        "p",
        "Toggle PTT (blocked while TX DISABLED)",
        Category::TxControl,
        false,
    ),
    kb(
        "g",
        "Cycle TX policy: Full → Respond-only → Disabled",
        Category::TxControl,
        false,
    ),
    kb(
        "v / V",
        "Cycle activity view: Operate/Hunt/Run/Monitor",
        Category::ModesViews,
        false,
    ),
    kb(
        "z",
        "Zoom focused panel (again/Esc to restore)",
        Category::ModesViews,
        false,
    ),
    kb("a", "Toggle autonomous mode", Category::ModesViews, true),
    kb(
        "Shift+P",
        "Pause / resume autonomous",
        Category::ModesViews,
        false,
    ),
    kb(
        "Shift+M",
        "Cycle operating mode (FT8 → FT4; waits for coordinator confirm)",
        Category::ModesViews,
        false,
    ),
    kb(
        "e",
        "Cycle decode-effort preset: Eco → Standard → Deep → Max → Auto",
        Category::ModesViews,
        false,
    ),
    kb(
        "Shift+H",
        "Engage Hound on selected DX Hunter station",
        Category::QsoOperation,
        false,
    ),
    kb(
        "Shift+X",
        "Toggle Fox (DXpedition) mode",
        Category::ModesViews,
        false,
    ),
    kb(
        "Shift+D",
        "Toggle Diagnostics overlay (retained event history)",
        Category::Diagnostics,
        false,
    ),
    kb(
        "Shift+S",
        "Toggle station-health panel (is the station healthy?)",
        Category::Diagnostics,
        false,
    ),
    kb(
        "Shift+R",
        "Toggle Recent-QSOs panel (retained terminal-QSO outcome history)",
        Category::Diagnostics,
        false,
    ),
    kb(
        "m",
        "Toggle audio monitoring",
        Category::AudioDevices,
        false,
    ),
    kb("d", "Device picker", Category::AudioDevices, true),
    kb(
        "x",
        "Clear decoded messages (press twice within 3s)",
        Category::Diagnostics,
        false,
    ),
    kb("q", "Quit (with confirm)", Category::Session, true),
    kb(
        "Shift+Q",
        "EMERGENCY STOP (halt TX, autonomous off)",
        Category::Session,
        true,
    ),
    kb(
        "Esc",
        "Dismiss overlay / cancel modal / clear stop banner",
        Category::Session,
        false,
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

    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for b in KEYBINDINGS {
            assert!(seen.insert(b.key), "duplicate keybinding entry: {}", b.key);
        }
    }
}
