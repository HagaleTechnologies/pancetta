//! Activity views (Phase 2 of the TUI redesign): a small 4-value ring the
//! operator cycles through with `v`/`V`. This module owns only the enum and
//! its pure methods (cycling, labeling, string round-trip) — the actual
//! per-view layout differences land in later tasks (5-7); until then every
//! view renders today's Operate layout.

use serde::{Deserialize, Serialize};

/// The operator's selected activity view. `Operate` is the default (today's
/// layout, unchanged) and is the only variant with no title-bar chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ActiveView {
    #[default]
    Operate,
    Hunt,
    Run,
    Monitor,
}

impl ActiveView {
    /// Advance to the next view in the ring, wrapping Monitor -> Operate.
    pub fn next(self) -> Self {
        match self {
            ActiveView::Operate => ActiveView::Hunt,
            ActiveView::Hunt => ActiveView::Run,
            ActiveView::Run => ActiveView::Monitor,
            ActiveView::Monitor => ActiveView::Operate,
        }
    }

    /// Step back to the previous view in the ring, wrapping Operate -> Monitor.
    pub fn prev(self) -> Self {
        match self {
            ActiveView::Operate => ActiveView::Monitor,
            ActiveView::Hunt => ActiveView::Operate,
            ActiveView::Run => ActiveView::Hunt,
            ActiveView::Monitor => ActiveView::Run,
        }
    }

    /// Title-bar chip label. `None` for Operate — the default view renders
    /// with no chip, so the title bar is byte-identical to today until the
    /// operator actually switches views.
    pub fn label(self) -> Option<&'static str> {
        match self {
            ActiveView::Operate => None,
            ActiveView::Hunt => Some("HUNT"),
            ActiveView::Run => Some("RUN"),
            ActiveView::Monitor => Some("MON"),
        }
    }

    /// Stable string form used for persistence (`~/.pancetta/tui_state.json`).
    pub fn as_str(self) -> &'static str {
        match self {
            ActiveView::Operate => "Operate",
            ActiveView::Hunt => "Hunt",
            ActiveView::Run => "Run",
            ActiveView::Monitor => "Monitor",
        }
    }

    /// Parse a persisted string, defaulting to `Operate` for anything
    /// unrecognized (missing file, garbage, or a future/older variant).
    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "Hunt" => ActiveView::Hunt,
            "Run" => ActiveView::Run,
            "Monitor" => ActiveView::Monitor,
            _ => ActiveView::Operate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_cycle_is_a_4_ring() {
        use ActiveView::*;
        assert_eq!(Operate.next(), Hunt);
        assert_eq!(Monitor.next(), Operate);
        assert_eq!(Operate.prev(), Monitor);
    }

    #[test]
    fn view_label_hidden_for_operate() {
        assert_eq!(ActiveView::Operate.label(), None);
        assert_eq!(ActiveView::Hunt.label(), Some("HUNT"));
        assert_eq!(ActiveView::Run.label(), Some("RUN"));
        assert_eq!(ActiveView::Monitor.label(), Some("MON"));
    }

    #[test]
    fn view_persistence_round_trip() {
        for v in [
            ActiveView::Operate,
            ActiveView::Hunt,
            ActiveView::Run,
            ActiveView::Monitor,
        ] {
            assert_eq!(ActiveView::from_str_or_default(v.as_str()), v);
        }
        assert_eq!(ActiveView::from_str_or_default("garbage"), ActiveView::Operate);
    }
}
