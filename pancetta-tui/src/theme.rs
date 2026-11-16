//! Semantic colour tokens for every TUI renderer (PAN-387).
//!
//! This is the only file in `pancetta-tui/src` that names a colour
//! (`pancetta-tui/tests/theme_invariant.rs` enforces it). Renderers read a
//! token by meaning (`app.theme.focus`, `theme.warning`, …) and never pick a
//! colour themselves, so a new panel cannot drift from the active theme.
//!
//! Three palettes exist:
//!
//! - [`Theme::DARK`] keeps the colours the TUI painted before PAN-387. Dark
//!   tokens carry colours only; call sites add the modifiers they always
//!   added. The one change is `page`, which also sets a white foreground so
//!   no cell falls back to the terminal's own colours.
//! - [`Theme::LIGHT`] is dark text on a white page; every token reaches
//!   3.0:1 on the xterm default palette.
//! - [`Theme::HIGH_CONTRAST`] reaches 4.5:1, gives each meaning a distinct
//!   style, makes every alarm bold and marks the selection with REVERSED.
//!
//! Every palette uses named ANSI-16 colours only, so a 16-colour SSH
//! session renders every theme. The waterfall intensity ramp is a data
//! palette, the same in every theme: [`waterfall_ramp`].

use ratatui::style::{Color, Modifier, Style};

use crate::app::ColorCapability;
use crate::widgets::{Occupancy, WaterfallColorScheme};

pub use pancetta_config::ui::ThemeName;

/// The semantic style tokens one theme paints with. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Which palette this is.
    pub name: ThemeName,
    /// Full-frame fill and default text.
    pub page: Style,
    /// Ordinary text.
    pub text: Style,
    /// De-emphasised text, hints, waterfall axis and labels.
    pub muted: Style,
    /// Neutral status: diagnostic Info, upload Unknown, benign QSO-end reasons.
    pub secondary: Style,
    /// Unfocused panel borders.
    pub border: Style,
    /// Unfocused panel titles and title-bar chrome.
    pub title: Style,
    /// Informational panel values.
    pub info: Style,
    /// Focused panel border and title, focused callsign, selected marker.
    pub focus: Style,
    /// A value to notice that is not a warning (dial frequency, quit prompt).
    pub emphasis: Style,
    /// Help overlay section headings.
    pub heading: Style,
    /// Key names in the footer and status hints.
    pub key: Style,
    /// Key column in the help overlay.
    pub help_key: Style,
    /// Good state.
    pub success: Style,
    /// Needs attention.
    pub warning: Style,
    /// Failure.
    pub error: Style,
    /// Readiness Notice chip and dot.
    pub notice: Style,
    /// A CQ message in Band Activity.
    pub cq: Style,
    /// A decode calling us.
    pub directed: Style,
    /// Our own transmission.
    pub own_tx: Style,
    /// A fresh callsign we can work.
    pub callable: Style,
    /// Our callsign in the title bar.
    pub own_call: Style,
    /// Already in the log on this band.
    pub worked: Style,
    /// Needed DXCC, grid or state.
    pub needed: Style,
    /// All-time new one.
    pub atno: Style,
    /// Station in a live QSO, stacked on the call style.
    pub engaged: Style,
    /// DX Hunter notable (`★`).
    pub notable: Style,
    /// Highlighted table row; selected row in the active picker list.
    pub selection: Style,
    /// Selected row in the inactive picker list.
    pub selection_inactive: Style,
    /// Readiness Blocker chip.
    pub chip_blocker: Style,
    /// Readiness Warning chip, frequency-drift badge.
    pub chip_warning: Style,
    /// Verdict HELD.
    pub verdict_held: Style,
    /// Verdict RX ONLY.
    pub verdict_rx_only: Style,
    /// Verdict WAIT.
    pub verdict_wait: Style,
    /// Verdict OPEN.
    pub verdict_open: Style,
    /// Verdict READY.
    pub verdict_ready: Style,
    /// `TX: FULL`.
    pub tx_full: Style,
    /// `TX: RESPOND-ONLY`.
    pub tx_respond_only: Style,
    /// `TX: DISABLED — RX ONLY`.
    pub tx_disabled: Style,
    /// `TX: REPLAY` (PAN-355: black on gray in every theme).
    pub tx_replay: Style,
    /// Active-view chip.
    pub view_chip: Style,
    /// `FOX` chip.
    pub fox: Style,
    /// Hound badge.
    pub hound: Style,
    /// `AUTO-CQ PAUSED` chip.
    pub cq_paused: Style,
    /// Live `TX` chip, TX NOW strip, ladder TX chip, stopped-by-operator banner.
    pub alarm: Style,
    /// Overlay surface.
    pub modal: Style,
    /// Overlay border and title.
    pub modal_border: Style,
    /// Active list border in the device and rig pickers.
    pub modal_focus: Style,
    /// Quit-confirm and out-of-band borders.
    pub modal_danger: Style,
    /// Waterfall strip and TX cursor: decode in our TX parity.
    pub occupancy_own: Style,
    /// Waterfall strip and TX cursor: decode in the other parity.
    pub occupancy_other: Style,
    /// Waterfall TX cursor on a clear column.
    pub occupancy_clear: Style,
    /// Waterfall decode markers.
    pub decode_tick: Style,
    /// Background of the waterfall's inner area.
    pub waterfall_data: Style,
}

const fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

const fn on(f: Color, b: Color) -> Style {
    Style::new().fg(f).bg(b)
}

const fn bg(c: Color) -> Style {
    Style::new().bg(c)
}

const B: Modifier = Modifier::BOLD;
const U: Modifier = Modifier::UNDERLINED;
const I: Modifier = Modifier::ITALIC;
const R: Modifier = Modifier::REVERSED;

const fn bold(s: Style) -> Style {
    s.add_modifier(B)
}

impl Theme {
    /// Today's dark palette (colours only; call sites add modifiers).
    pub const DARK: Theme = Theme {
        name: ThemeName::Dark,
        page: on(Color::White, Color::Black),
        text: fg(Color::White),
        muted: fg(Color::DarkGray),
        secondary: fg(Color::Gray),
        border: fg(Color::Gray),
        title: fg(Color::Cyan),
        info: fg(Color::Cyan),
        focus: fg(Color::Yellow),
        emphasis: fg(Color::Yellow),
        heading: fg(Color::Cyan),
        key: fg(Color::Cyan),
        help_key: fg(Color::Yellow),
        success: fg(Color::Green),
        warning: fg(Color::Yellow),
        error: fg(Color::Red),
        notice: fg(Color::Cyan),
        cq: fg(Color::Yellow),
        directed: fg(Color::Yellow),
        own_tx: fg(Color::Red),
        callable: fg(Color::Green),
        own_call: fg(Color::Green),
        worked: fg(Color::DarkGray),
        needed: fg(Color::Red),
        atno: fg(Color::Magenta),
        engaged: fg(Color::Green),
        notable: fg(Color::Magenta),
        selection: on(Color::Black, Color::Cyan),
        selection_inactive: on(Color::White, Color::DarkGray),
        chip_blocker: on(Color::White, Color::Red),
        chip_warning: on(Color::Black, Color::Yellow),
        verdict_held: on(Color::White, Color::Red),
        verdict_rx_only: on(Color::Black, Color::White),
        verdict_wait: on(Color::Black, Color::Cyan),
        verdict_open: on(Color::Black, Color::Yellow),
        verdict_ready: on(Color::Black, Color::Green),
        tx_full: on(Color::Black, Color::Green),
        tx_respond_only: on(Color::Black, Color::Yellow),
        tx_disabled: on(Color::Black, Color::Red),
        tx_replay: on(Color::Black, Color::Gray),
        view_chip: on(Color::Black, Color::White),
        fox: on(Color::Black, Color::Magenta),
        hound: on(Color::Black, Color::Cyan),
        cq_paused: on(Color::Black, Color::Yellow),
        alarm: on(Color::White, Color::Red),
        modal: on(Color::White, Color::Black),
        modal_border: fg(Color::Cyan),
        modal_focus: fg(Color::Cyan),
        modal_danger: fg(Color::Red),
        occupancy_own: fg(Color::Red),
        occupancy_other: fg(Color::Yellow),
        occupancy_clear: fg(Color::Green),
        decode_tick: fg(Color::Yellow),
        waterfall_data: bg(Color::Black),
    };

    /// Dark text on a white page; every token reaches 3.0:1.
    pub const LIGHT: Theme = Theme {
        name: ThemeName::Light,
        page: on(Color::Black, Color::White),
        text: fg(Color::Black),
        muted: fg(Color::DarkGray),
        secondary: fg(Color::DarkGray),
        border: fg(Color::DarkGray),
        title: fg(Color::Blue),
        info: fg(Color::Blue),
        focus: bold(fg(Color::Magenta)),
        emphasis: bold(fg(Color::Black)),
        heading: fg(Color::Black).add_modifier(U),
        key: fg(Color::Blue),
        help_key: fg(Color::Blue),
        success: fg(Color::Blue),
        warning: on(Color::Black, Color::Yellow),
        error: bold(fg(Color::Red)),
        notice: bold(fg(Color::LightBlue)),
        cq: bold(fg(Color::Blue)),
        directed: bold(on(Color::White, Color::Blue)),
        own_tx: bold(on(Color::White, Color::Red)),
        callable: bold(fg(Color::Black)),
        own_call: bold(fg(Color::Black)),
        worked: fg(Color::DarkGray),
        needed: bold(fg(Color::Red)).add_modifier(U),
        atno: bold(fg(Color::Magenta)).add_modifier(U),
        engaged: fg(Color::Blue).add_modifier(U),
        notable: bold(fg(Color::Magenta)).add_modifier(I),
        selection: Style::new().add_modifier(R),
        selection_inactive: Style::new().add_modifier(U),
        chip_blocker: bold(on(Color::White, Color::Red)),
        chip_warning: bold(on(Color::Black, Color::Yellow)),
        verdict_held: bold(on(Color::White, Color::Red)),
        verdict_rx_only: bold(on(Color::White, Color::Black)),
        verdict_wait: bold(on(Color::Black, Color::Cyan)),
        verdict_open: bold(on(Color::Black, Color::Yellow)),
        verdict_ready: bold(on(Color::Black, Color::Green)),
        tx_full: bold(on(Color::Black, Color::Green)),
        tx_respond_only: bold(on(Color::Black, Color::Yellow)),
        tx_disabled: bold(on(Color::White, Color::Red)),
        tx_replay: on(Color::Black, Color::Gray),
        view_chip: bold(on(Color::White, Color::Black)),
        fox: bold(on(Color::White, Color::Magenta)),
        hound: bold(on(Color::Black, Color::Cyan)),
        cq_paused: bold(on(Color::Black, Color::Yellow)),
        alarm: bold(on(Color::White, Color::Red)),
        modal: on(Color::Black, Color::White),
        modal_border: bold(fg(Color::Blue)),
        modal_focus: bold(fg(Color::Blue)),
        modal_danger: bold(fg(Color::Red)),
        occupancy_own: fg(Color::Red),
        occupancy_other: fg(Color::Yellow),
        occupancy_clear: fg(Color::Green),
        decode_tick: fg(Color::Yellow),
        waterfall_data: bg(Color::Black),
    };

    /// 4.5:1 or better, distinct styles per meaning, bold alarms.
    pub const HIGH_CONTRAST: Theme = Theme {
        name: ThemeName::HighContrast,
        page: on(Color::White, Color::Black),
        text: fg(Color::White),
        muted: fg(Color::Gray),
        secondary: fg(Color::Gray),
        border: fg(Color::Gray),
        title: fg(Color::LightCyan),
        info: fg(Color::Cyan),
        focus: bold(on(Color::Black, Color::LightYellow)),
        emphasis: bold(fg(Color::LightYellow)),
        heading: bold(fg(Color::LightCyan)).add_modifier(U),
        key: bold(fg(Color::LightYellow)),
        help_key: bold(fg(Color::LightYellow)),
        success: bold(fg(Color::LightGreen)),
        warning: bold(fg(Color::Yellow)),
        error: bold(fg(Color::LightRed)),
        notice: bold(fg(Color::LightCyan)),
        cq: bold(fg(Color::Cyan)),
        directed: bold(on(Color::Black, Color::LightGreen)),
        own_tx: bold(on(Color::Black, Color::LightRed)),
        callable: bold(fg(Color::Green)),
        own_call: bold(fg(Color::LightGreen)),
        worked: fg(Color::Gray),
        needed: bold(fg(Color::LightRed)).add_modifier(U),
        atno: bold(fg(Color::LightMagenta)).add_modifier(U),
        engaged: fg(Color::LightGreen).add_modifier(U),
        notable: bold(fg(Color::LightMagenta)),
        selection: Style::new().add_modifier(R).add_modifier(B),
        selection_inactive: Style::new().add_modifier(U),
        chip_blocker: bold(on(Color::White, Color::Red)),
        chip_warning: bold(on(Color::Black, Color::Yellow)),
        verdict_held: bold(on(Color::White, Color::Red)),
        verdict_rx_only: bold(on(Color::Black, Color::White)),
        verdict_wait: bold(on(Color::Black, Color::Cyan)),
        verdict_open: bold(on(Color::Black, Color::Yellow)),
        verdict_ready: bold(on(Color::Black, Color::Green)),
        tx_full: bold(on(Color::Black, Color::Green)),
        tx_respond_only: bold(on(Color::Black, Color::Yellow)),
        tx_disabled: bold(on(Color::White, Color::Red)),
        tx_replay: on(Color::Black, Color::Gray),
        view_chip: bold(on(Color::Black, Color::White)),
        fox: bold(on(Color::Black, Color::LightMagenta)),
        hound: bold(on(Color::Black, Color::Cyan)),
        cq_paused: bold(on(Color::Black, Color::Yellow)),
        alarm: bold(on(Color::White, Color::Red)),
        modal: on(Color::White, Color::Black),
        modal_border: bold(fg(Color::White)),
        modal_focus: bold(fg(Color::LightYellow)),
        modal_danger: bold(fg(Color::LightRed)),
        occupancy_own: fg(Color::LightRed),
        occupancy_other: fg(Color::Yellow),
        occupancy_clear: fg(Color::LightGreen),
        decode_tick: fg(Color::LightYellow),
        waterfall_data: bg(Color::Black),
    };

    /// The palette for a theme name.
    pub const fn for_name(name: ThemeName) -> Theme {
        match name {
            ThemeName::Dark => Theme::DARK,
            ThemeName::Light => Theme::LIGHT,
            ThemeName::HighContrast => Theme::HIGH_CONTRAST,
        }
    }

    /// The `engaged` highlight (a station in a live QSO) stacked on a
    /// callsign style. A base that paints its own background (Light
    /// `directed` and `own_tx`, HighContrast `focus`) keeps its colours and
    /// gains only `engaged`'s modifiers, so the stack never draws `engaged`'s
    /// foreground on a background it was not contrast-checked against (Light
    /// Blue on `directed`'s Blue). No Dark callsign base sets a background, so
    /// Dark keeps today's `patch`.
    pub fn engaged_on(&self, base: Style) -> Style {
        if base.bg.is_some() {
            base.add_modifier(self.engaged.add_modifier)
        } else {
            base.patch(self.engaged)
        }
    }

    /// SNR scale (`docs/DECISIONS/tui.md`): SNR ≥ 0 `success`, -10 to -1
    /// `warning`, below -10 `error`.
    pub fn snr(&self, snr: i32) -> Style {
        if snr >= 0 {
            self.success
        } else if snr >= -10 {
            self.warning
        } else {
            self.error
        }
    }

    /// Audio input level (0.0..=1.0): above 0.8 `error` (clipping), above
    /// 0.1 `success`, above 0.01 `warning`, otherwise `muted`.
    pub fn audio_level(&self, level: f32) -> Style {
        if level > 0.8 {
            self.error
        } else if level > 0.1 {
            self.success
        } else if level > 0.01 {
            self.warning
        } else {
            self.muted
        }
    }

    /// The waterfall occupancy style for a column's occupancy state.
    pub fn occupancy(&self, occupancy: Occupancy) -> Style {
        match occupancy {
            Occupancy::Clear => self.occupancy_clear,
            Occupancy::OtherParity => self.occupancy_other,
            Occupancy::OwnParity => self.occupancy_own,
        }
    }

    /// Every token with its name, in declaration order (53 tokens).
    pub fn tokens(&self) -> [(&'static str, Style); 53] {
        [
            ("page", self.page),
            ("text", self.text),
            ("muted", self.muted),
            ("secondary", self.secondary),
            ("border", self.border),
            ("title", self.title),
            ("info", self.info),
            ("focus", self.focus),
            ("emphasis", self.emphasis),
            ("heading", self.heading),
            ("key", self.key),
            ("help_key", self.help_key),
            ("success", self.success),
            ("warning", self.warning),
            ("error", self.error),
            ("notice", self.notice),
            ("cq", self.cq),
            ("directed", self.directed),
            ("own_tx", self.own_tx),
            ("callable", self.callable),
            ("own_call", self.own_call),
            ("worked", self.worked),
            ("needed", self.needed),
            ("atno", self.atno),
            ("engaged", self.engaged),
            ("notable", self.notable),
            ("selection", self.selection),
            ("selection_inactive", self.selection_inactive),
            ("chip_blocker", self.chip_blocker),
            ("chip_warning", self.chip_warning),
            ("verdict_held", self.verdict_held),
            ("verdict_rx_only", self.verdict_rx_only),
            ("verdict_wait", self.verdict_wait),
            ("verdict_open", self.verdict_open),
            ("verdict_ready", self.verdict_ready),
            ("tx_full", self.tx_full),
            ("tx_respond_only", self.tx_respond_only),
            ("tx_disabled", self.tx_disabled),
            ("tx_replay", self.tx_replay),
            ("view_chip", self.view_chip),
            ("fox", self.fox),
            ("hound", self.hound),
            ("cq_paused", self.cq_paused),
            ("alarm", self.alarm),
            ("modal", self.modal),
            ("modal_border", self.modal_border),
            ("modal_focus", self.modal_focus),
            ("modal_danger", self.modal_danger),
            ("occupancy_own", self.occupancy_own),
            ("occupancy_other", self.occupancy_other),
            ("occupancy_clear", self.occupancy_clear),
            ("decode_tick", self.decode_tick),
            ("waterfall_data", self.waterfall_data),
        ]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::DARK
    }
}

/// Keep only the foreground channel (and the modifiers) of a token, for a
/// call site that sets the foreground alone.
pub const fn fg_of(token: Style) -> Style {
    let mut s = token;
    s.bg = None;
    s
}

/// Keep only the background channel of a token, for a call site that sets
/// the background alone.
pub const fn bg_of(token: Style) -> Style {
    let mut s = Style::new();
    s.bg = token.bg;
    s
}

/// The foreground colour a token paints, `Reset` when it sets none. For
/// widgets and tests that need a bare colour rather than a style.
pub fn fg_color(token: Style) -> Color {
    token.fg.unwrap_or(Color::Reset)
}

/// The background colour a token paints, `Reset` when it sets none.
pub fn bg_color(token: Style) -> Color {
    token.bg.unwrap_or(Color::Reset)
}

/// The waterfall intensity ramp. A data palette, the same in every theme:
/// 256-colour terminals get the chosen scheme's ramp; 16-colour terminals
/// get bright, always-visible colours paired with density glyphs (the glyph
/// encodes intensity, so the lowest band is never the page colour).
pub fn waterfall_ramp(
    scheme: WaterfallColorScheme,
    capability: ColorCapability,
    intensity: f32,
) -> Color {
    let clamped = intensity.clamp(0.0, 1.0);

    if capability == ColorCapability::Basic {
        return if clamped < 0.30 {
            Color::Gray
        } else if clamped < 0.55 {
            Color::Cyan
        } else if clamped < 0.80 {
            Color::White
        } else {
            Color::Yellow
        };
    }

    match scheme {
        WaterfallColorScheme::Classic => {
            // Luminance-based: dark = quiet, bright = loud.
            // Colorblind-safe — relies on brightness, not hue.
            // Uses 256-color grayscale ramp (232–255) for smooth gradient.
            // 232 = #080808 (near-black), 255 = #eeeeee (near-white)
            let idx = 232 + (clamped * 23.0) as u8;
            Color::Indexed(idx)
        }
        WaterfallColorScheme::Spectrum => {
            // Blue-to-white luminance ramp (colorblind-safe)
            // Uses 256-color: dark blue → blue → cyan → white
            if clamped < 0.25 {
                Color::Indexed(17) // dark blue
            } else if clamped < 0.5 {
                Color::Indexed(19) // medium blue
            } else if clamped < 0.75 {
                Color::Indexed(33) // bright blue
            } else {
                Color::Indexed(255) // white
            }
        }
        WaterfallColorScheme::Thermal => {
            // Black → dark → bright (pure luminance)
            if clamped < 0.33 {
                Color::Black
            } else if clamped < 0.66 {
                Color::DarkGray
            } else {
                Color::White
            }
        }
    }
}

/// The xterm default-palette RGB of a colour; `None` for `Reset`.
pub fn xterm_rgb(color: Color) -> Option<(u8, u8, u8)> {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let index = match color {
        Color::Reset => return None,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Rgb(r, g, b) => return Some((r, g, b)),
        Color::Indexed(i) => {
            if i < 16 {
                i
            } else if i < 232 {
                let n = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                return Some((level(n / 36), level((n / 6) % 6), level(n % 6)));
            } else {
                let v = 8 + (i - 232) * 10;
                return Some((v, v, v));
            }
        }
    };
    Some(ANSI[index as usize])
}

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG 2.x contrast ratio between two colours on the xterm default
/// palette; `None` when either is `Reset` (the terminal decides).
pub fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
    let la = relative_luminance(xterm_rgb(a)?);
    let lb = relative_luminance(xterm_rgb(b)?);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// Whether a colour is chosen by the theme rather than left to the terminal.
pub fn is_explicit(color: Color) -> bool {
    color != Color::Reset
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEANING_TOKENS: [&str; 14] = [
        "focus", "success", "warning", "error", "notice", "cq", "directed", "own_tx", "callable",
        "worked", "needed", "atno", "engaged", "notable",
    ];

    const WATERFALL_TOKENS: [&str; 4] = [
        "occupancy_own",
        "occupancy_other",
        "occupancy_clear",
        "decode_tick",
    ];

    fn is_named_ansi16(c: Color) -> bool {
        !matches!(c, Color::Indexed(_) | Color::Rgb(..) | Color::Reset)
    }

    /// The background a token's text sits on: its own, the waterfall panel
    /// for waterfall tokens, else the palette's page.
    fn effective_bg(theme: &Theme, name: &str, token: Style) -> Color {
        if let Some(bg) = token.bg {
            return bg;
        }
        if WATERFALL_TOKENS.contains(&name) {
            return bg_color(theme.waterfall_data);
        }
        bg_color(theme.page)
    }

    fn effective_fg(theme: &Theme, token: Style) -> Option<Color> {
        if token.add_modifier.contains(Modifier::REVERSED) && token.fg.is_none() {
            return Some(fg_color(theme.page));
        }
        token.fg.or(theme.page.fg)
    }

    fn min_token_contrast(theme: &Theme) -> Vec<(&'static str, f64)> {
        theme
            .tokens()
            .iter()
            .filter(|(name, _)| *name != "waterfall_data")
            .map(|(name, token)| {
                let f = effective_fg(theme, *token).expect("fg");
                let b = effective_bg(theme, name, *token);
                (*name, contrast_ratio(f, b).expect("explicit colours"))
            })
            .collect()
    }

    #[test]
    fn for_name_returns_each_palette() {
        assert_eq!(Theme::for_name(ThemeName::Dark), Theme::DARK);
        assert_eq!(Theme::for_name(ThemeName::Light), Theme::LIGHT);
        assert_eq!(
            Theme::for_name(ThemeName::HighContrast),
            Theme::HIGH_CONTRAST
        );
        for name in ThemeName::ALL {
            assert_eq!(Theme::for_name(name).name, name);
        }
        assert_eq!(Theme::default(), Theme::DARK);
    }

    #[test]
    fn tokens_use_only_named_ansi16_colours() {
        for name in ThemeName::ALL {
            let theme = Theme::for_name(name);
            for (token, style) in theme.tokens() {
                for c in [style.fg, style.bg].into_iter().flatten() {
                    assert!(is_named_ansi16(c), "{name:?}.{token} uses {c:?}");
                }
            }
        }
    }

    /// Every Dark token against the colour the pre-PAN-387 code painted for
    /// that meaning (the file it came from), so Dark keeps today's look. The
    /// one change is `page`, which gains a white foreground.
    #[test]
    fn dark_tokens_match_legacy_dark_colours() {
        use Color::*;
        let n = None;
        let legacy: [(&str, Option<Color>, Option<Color>, &str); 53] = [
            (
                "page",
                Some(White),
                Some(Black),
                "config.rs background_color/foreground_color (Dark)",
            ),
            ("text", Some(White), n, "config.rs foreground_color (Dark)"),
            ("muted", Some(DarkGray), n, "config.rs muted_color (Dark)"),
            (
                "secondary",
                Some(Gray),
                n,
                "ui/mod.rs diagnostics Info, upload Unknown",
            ),
            ("border", Some(Gray), n, "config.rs border_color (Dark)"),
            (
                "title",
                Some(Cyan),
                n,
                "config.rs accent_color (Dark), ui/mod.rs title bar",
            ),
            (
                "info",
                Some(Cyan),
                n,
                "ui/dx_hunter.rs Network/Both source, accent_color",
            ),
            ("focus", Some(Yellow), n, "config.rs selected_color (Dark)"),
            (
                "emphasis",
                Some(Yellow),
                n,
                "ui/mod.rs dial frequency, tui_runner.rs quit prompt",
            ),
            (
                "heading",
                Some(Cyan),
                n,
                "tui_runner.rs help_body_lines headings",
            ),
            ("key", Some(Cyan), n, "ui/mod.rs render_status_bar keys"),
            (
                "help_key",
                Some(Yellow),
                n,
                "tui_runner.rs help_body_lines key column",
            ),
            ("success", Some(Green), n, "config.rs success_color"),
            ("warning", Some(Yellow), n, "config.rs warning_color"),
            ("error", Some(Red), n, "config.rs error_color"),
            ("notice", Some(Cyan), n, "ui/readiness_strip.rs Notice chip"),
            (
                "cq",
                Some(Yellow),
                n,
                "ui/band_activity.rs CQ message (warning_color)",
            ),
            (
                "directed",
                Some(Yellow),
                n,
                "ui/band_activity.rs calling-us row (selected_color)",
            ),
            (
                "own_tx",
                Some(Red),
                n,
                "ui/band_activity.rs own TX row (error_color)",
            ),
            (
                "callable",
                Some(Green),
                n,
                "ui/band_activity.rs fresh callsign (success_color)",
            ),
            ("own_call", Some(Green), n, "ui/mod.rs title bar callsign"),
            (
                "worked",
                Some(DarkGray),
                n,
                "ui/band_activity.rs worked call (muted_color)",
            ),
            ("needed", Some(Red), n, "ui/need_display.rs needed_style"),
            ("atno", Some(Magenta), n, "ui/need_display.rs atno_style"),
            (
                "engaged",
                Some(Green),
                n,
                "ui/band_activity.rs in-QSO station",
            ),
            ("notable", Some(Magenta), n, "ui/dx_hunter.rs notable star"),
            (
                "selection",
                Some(Black),
                Some(Cyan),
                "ui/band_activity.rs row_highlight_style",
            ),
            (
                "selection_inactive",
                Some(White),
                Some(DarkGray),
                "tui_runner.rs device picker inactive list",
            ),
            (
                "chip_blocker",
                Some(White),
                Some(Red),
                "ui/readiness_strip.rs Blocker chip",
            ),
            (
                "chip_warning",
                Some(Black),
                Some(Yellow),
                "ui/readiness_strip.rs Warning chip",
            ),
            (
                "verdict_held",
                Some(White),
                Some(Red),
                "ui/readiness_strip.rs verdict HELD",
            ),
            (
                "verdict_rx_only",
                Some(Black),
                Some(White),
                "ui/readiness_strip.rs verdict RX ONLY",
            ),
            (
                "verdict_wait",
                Some(Black),
                Some(Cyan),
                "ui/readiness_strip.rs verdict WAIT",
            ),
            (
                "verdict_open",
                Some(Black),
                Some(Yellow),
                "ui/readiness_strip.rs verdict OPEN",
            ),
            (
                "verdict_ready",
                Some(Black),
                Some(Green),
                "ui/readiness_strip.rs verdict READY",
            ),
            (
                "tx_full",
                Some(Black),
                Some(Green),
                "ui/mod.rs title_bar_segments TX FULL",
            ),
            (
                "tx_respond_only",
                Some(Black),
                Some(Yellow),
                "ui/mod.rs title_bar_segments RESPOND-ONLY",
            ),
            (
                "tx_disabled",
                Some(Black),
                Some(Red),
                "ui/mod.rs title_bar_segments DISABLED",
            ),
            (
                "tx_replay",
                Some(Black),
                Some(Gray),
                "ui/mod.rs title_bar_segments REPLAY (PAN-355)",
            ),
            (
                "view_chip",
                Some(Black),
                Some(White),
                "ui/mod.rs title_bar_segments view chip",
            ),
            (
                "fox",
                Some(Black),
                Some(Magenta),
                "ui/mod.rs title_bar_segments FOX",
            ),
            (
                "hound",
                Some(Black),
                Some(Cyan),
                "ui/qso_status.rs hound badge",
            ),
            (
                "cq_paused",
                Some(Black),
                Some(Yellow),
                "ui/mod.rs title_bar_segments AUTO-CQ PAUSED",
            ),
            (
                "alarm",
                Some(White),
                Some(Red),
                "ui/mod.rs TX chip / tui_runner.rs stopped banner",
            ),
            (
                "modal",
                Some(White),
                Some(Black),
                "tui_runner.rs quit confirm / help surface",
            ),
            (
                "modal_border",
                Some(Cyan),
                n,
                "ui/mod.rs freq/offset/watchlist modal borders",
            ),
            (
                "modal_focus",
                Some(Cyan),
                n,
                "tui_runner.rs device picker active list border",
            ),
            (
                "modal_danger",
                Some(Red),
                n,
                "tui_runner.rs quit confirm, ui/mod.rs out-of-band",
            ),
            (
                "occupancy_own",
                Some(Red),
                n,
                "widgets/mod.rs occupancy strip, own parity",
            ),
            (
                "occupancy_other",
                Some(Yellow),
                n,
                "widgets/mod.rs occupancy strip, other parity",
            ),
            (
                "occupancy_clear",
                Some(Green),
                n,
                "widgets/mod.rs TX cursor clear column",
            ),
            (
                "decode_tick",
                Some(Yellow),
                n,
                "widgets/mod.rs decode markers",
            ),
            (
                "waterfall_data",
                n,
                Some(Black),
                "widgets/mod.rs waterfall area (dark page)",
            ),
        ];
        let tokens = Theme::DARK.tokens();
        for ((name, style), (lname, lfg, lbg, source)) in tokens.iter().zip(legacy.iter()) {
            assert_eq!(name, lname, "token order");
            assert_eq!(style.fg, *lfg, "{name} fg (from {source})");
            assert_eq!(style.bg, *lbg, "{name} bg (from {source})");
            assert!(
                style.add_modifier.is_empty(),
                "Dark {name} carries colours only"
            );
        }
    }

    #[test]
    fn snr_scale_matches_the_recorded_decision() {
        for name in ThemeName::ALL {
            let t = Theme::for_name(name);
            assert_eq!(t.snr(0), t.success);
            assert_eq!(t.snr(5), t.success);
            assert_eq!(t.snr(-1), t.warning);
            assert_eq!(t.snr(-10), t.warning);
            assert_eq!(t.snr(-11), t.error);
        }
    }

    #[test]
    fn replay_chip_is_black_on_gray_in_every_theme() {
        for name in ThemeName::ALL {
            let t = Theme::for_name(name);
            assert_eq!(t.tx_replay.fg, Some(Color::Black), "{name:?}");
            assert_eq!(t.tx_replay.bg, Some(Color::Gray), "{name:?}");
        }
    }

    #[test]
    fn audio_level_thresholds_are_unchanged() {
        for name in ThemeName::ALL {
            let t = Theme::for_name(name);
            assert_eq!(t.audio_level(0.9), t.error);
            assert_eq!(t.audio_level(0.5), t.success);
            assert_eq!(t.audio_level(0.05), t.warning);
            assert_eq!(t.audio_level(0.0), t.muted);
        }
    }

    #[test]
    fn atno_stays_bold_and_underlined_where_modifiers_live() {
        for t in [Theme::LIGHT, Theme::HIGH_CONTRAST] {
            assert!(t
                .atno
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED));
        }
    }

    #[test]
    fn contrast_ratio_matches_wcag_reference_values() {
        let bw = contrast_ratio(Color::Black, Color::White).unwrap();
        assert!((bw - 21.0).abs() < 1e-9, "{bw}");
        let rb = contrast_ratio(Color::Red, Color::Black).unwrap();
        assert_eq!(format!("{rb:.1}"), "3.6");
        assert_eq!(
            contrast_ratio(Color::Red, Color::Black),
            contrast_ratio(Color::Black, Color::Red)
        );
        assert_eq!(contrast_ratio(Color::Reset, Color::Black), None);
        assert!(!is_explicit(Color::Reset));
        assert!(is_explicit(Color::Black));
    }

    #[test]
    fn light_tokens_meet_3_to_1() {
        for (name, ratio) in min_token_contrast(&Theme::LIGHT) {
            assert!(ratio >= 3.0, "Light {name} is {ratio:.2}:1");
        }
    }

    #[test]
    fn high_contrast_tokens_meet_4_5_to_1() {
        for (name, ratio) in min_token_contrast(&Theme::HIGH_CONTRAST) {
            assert!(ratio >= 4.5, "HighContrast {name} is {ratio:.2}:1");
        }
    }

    fn meaning_tokens(theme: &Theme) -> Vec<(&'static str, Style)> {
        theme
            .tokens()
            .into_iter()
            .filter(|(name, _)| MEANING_TOKENS.contains(name))
            .collect()
    }

    #[test]
    fn high_contrast_meaning_tokens_are_distinct() {
        for theme in [Theme::HIGH_CONTRAST, Theme::LIGHT] {
            let tokens = meaning_tokens(&theme);
            assert_eq!(tokens.len(), 14);
            for (i, (a, sa)) in tokens.iter().enumerate() {
                for (b, sb) in tokens.iter().skip(i + 1) {
                    assert_ne!(sa, sb, "{:?}: {a} and {b} share a style", theme.name);
                }
            }
        }
    }

    #[test]
    fn high_contrast_alarm_tokens_are_bold() {
        let t = Theme::HIGH_CONTRAST;
        for (name, style) in [
            ("error", t.error),
            ("needed", t.needed),
            ("own_tx", t.own_tx),
            ("chip_blocker", t.chip_blocker),
            ("verdict_held", t.verdict_held),
            ("tx_disabled", t.tx_disabled),
            ("alarm", t.alarm),
            ("modal_danger", t.modal_danger),
        ] {
            assert!(style.add_modifier.contains(Modifier::BOLD), "{name}");
        }
    }

    /// Review fix: `engaged` stacked on a filled callsign base (Light
    /// `directed`, HighContrast `focus`) must stay readable, and Dark must
    /// keep today's plain `patch`.
    #[test]
    fn engaged_stack_keeps_contrast_on_every_callsign_base() {
        for (theme, min) in [
            (Theme::DARK, None),
            (Theme::LIGHT, Some(3.0)),
            (Theme::HIGH_CONTRAST, Some(4.5)),
        ] {
            for (name, base) in [
                ("directed", theme.directed),
                ("own_tx", theme.own_tx),
                ("focus", theme.focus),
                ("needed", theme.needed),
                ("atno", theme.atno),
                ("callable", theme.callable),
                ("emphasis", theme.emphasis),
                ("info", theme.info),
                ("muted", theme.muted),
            ] {
                let stacked = theme.engaged_on(base);
                let Some(min) = min else {
                    assert_eq!(stacked, base.patch(theme.engaged), "dark {name}");
                    continue;
                };
                let fg = fg_color(stacked);
                let bg = stacked.bg.or(theme.page.bg).unwrap_or(Color::Reset);
                let ratio = contrast_ratio(fg, bg).unwrap_or(0.0);
                assert!(
                    ratio >= min,
                    "{} engaged on {name}: {fg:?} on {bg:?} = {ratio:.2}",
                    theme.name.as_str()
                );
            }
        }
    }

    #[test]
    fn high_contrast_selection_is_reversed() {
        let s = Theme::HIGH_CONTRAST.selection;
        assert!(s.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(s.fg, None);
        assert_eq!(s.bg, None);
    }

    #[test]
    fn test_theme_colors() {
        assert_eq!(Theme::DARK.page.bg, Some(Color::Black));
        assert_eq!(Theme::LIGHT.page.bg, Some(Color::White));
        assert_eq!(Theme::DARK.text.fg, Some(Color::White));
        assert_eq!(Theme::LIGHT.text.fg, Some(Color::Black));
    }

    #[test]
    fn fg_of_and_bg_of_keep_one_channel() {
        let t = Theme::DARK.selection;
        assert_eq!(fg_of(t), Style::new().fg(Color::Black));
        assert_eq!(bg_of(t), Style::new().bg(Color::Cyan));
    }

    #[test]
    fn test_waterfall_color_intensity() {
        use WaterfallColorScheme::*;
        let full = ColorCapability::TwoFiftySix;
        assert_eq!(waterfall_ramp(Classic, full, 0.0), Color::Indexed(232));
        assert_eq!(waterfall_ramp(Classic, full, 1.0), Color::Indexed(255));
        assert_eq!(waterfall_ramp(Spectrum, full, 0.1), Color::Indexed(17));
        assert_eq!(waterfall_ramp(Spectrum, full, 0.9), Color::Indexed(255));
        assert_eq!(waterfall_ramp(Thermal, full, 0.1), Color::Black);
        assert_eq!(waterfall_ramp(Thermal, full, 0.9), Color::White);
        let basic = ColorCapability::Basic;
        assert_eq!(waterfall_ramp(Classic, basic, 0.0), Color::Gray);
        assert_eq!(waterfall_ramp(Classic, basic, 0.4), Color::Cyan);
        assert_eq!(waterfall_ramp(Classic, basic, 0.7), Color::White);
        assert_eq!(waterfall_ramp(Classic, basic, 0.95), Color::Yellow);
    }
}
