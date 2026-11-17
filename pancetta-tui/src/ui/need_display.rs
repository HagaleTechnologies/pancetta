//! Shared need glyphs and styles. Renderers only consume prepared verdicts.
use ratatui::style::{Modifier, Style};

use crate::theme::Theme;

/// Build the callsign-prefix marker string for the DX Hunter:
/// `!` for ATNO, `+` for a needed (non-ATNO) DXCC, then `★` for notable.
/// ATNO and needed are mutually exclusive (ATNO wins). Returns `""` for a
/// plain station.
pub(super) fn need_marker(atno: bool, needed: bool, notable: bool) -> String {
    let mut s = String::new();
    if atno {
        s.push('!');
    } else if needed {
        s.push('+');
    }
    if notable {
        s.push('★');
    }
    s
}

/// ATNO already implies a new entity on this band.
pub(super) fn band_marker(band_needed: bool, atno: bool) -> &'static str {
    if band_needed && !atno {
        "▲"
    } else {
        ""
    }
}

pub(super) fn atno_style(theme: &Theme) -> Style {
    theme
        .atno
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

pub(super) fn needed_style(theme: &Theme) -> Style {
    theme.needed.add_modifier(Modifier::BOLD)
}
