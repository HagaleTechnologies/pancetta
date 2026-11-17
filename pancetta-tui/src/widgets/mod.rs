use ratatui::{layout::Rect, widgets::Block};

use crate::app::ColorCapability;
use crate::theme::{self, Theme};
pub use pancetta_config::ui::WaterfallPalette;

/// The occupancy state of one waterfall column (PAN-387): the decision is
/// data, the colour comes from the theme, so the TX-cursor suffix stays right
/// even for a theme that paints two states alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// No decode within ±37.5 Hz in the last 60 s.
    Clear,
    /// A decode in the OTHER parity (or own parity unknown).
    OtherParity,
    /// A decode in YOUR TX parity.
    OwnParity,
}

/// Classify a column of the occupancy strip by recent decode activity.
/// `OwnParity` = decode within ±37.5 Hz in YOUR TX parity in the last 60s.
/// `OtherParity` = decode within ±37.5 Hz in the OTHER parity (or own-parity unknown).
/// `Clear` = no decodes nearby in the last 60s.
///
/// `decoded` is `(frequency_hz, parity, timestamp)`. `tx_parity = None`
/// (operator's parity unknown — Auto + idle) collapses `OwnParity` to
/// `OtherParity` because we can't say if a decode would collide.
pub(crate) fn occupancy(
    col: usize,
    width: usize,
    freq_range: (f64, f64),
    decoded: &[(
        f64,
        pancetta_core::slot::SlotParity,
        chrono::DateTime<chrono::Utc>,
    )],
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    now: chrono::DateTime<chrono::Utc>,
) -> Occupancy {
    if width == 0 {
        return Occupancy::Clear;
    }
    let (lo, hi) = freq_range;
    let bin_hz = (hi - lo) / width as f64;
    let center = lo + (col as f64 + 0.5) * bin_hz;
    let cutoff = now - chrono::Duration::seconds(60);

    let in_band = |d: &(
        f64,
        pancetta_core::slot::SlotParity,
        chrono::DateTime<chrono::Utc>,
    )| { (d.0 - center).abs() <= 37.5 && d.2 >= cutoff };

    let any_in_band = decoded.iter().any(in_band);
    if !any_in_band {
        return Occupancy::Clear;
    }

    match tx_parity {
        Some(my_parity) => {
            let busy_own = decoded.iter().any(|d| in_band(d) && d.1 == my_parity);
            if busy_own {
                Occupancy::OwnParity
            } else {
                Occupancy::OtherParity
            }
        }
        None => Occupancy::OtherParity,
    }
}

/// Custom waterfall widget for displaying spectrum data
pub struct Waterfall<'a> {
    block: Option<Block<'a>>,
    data: &'a [Vec<f32>],
    /// Colour palette (`[ui.waterfall] palette`, PAN-389). Ignored on
    /// 16-colour terminals, which encode intensity in density glyphs.
    palette: WaterfallPalette,
    /// Display gain in dB (`[ui.waterfall] gain_db`, PAN-389). Added at render
    /// time on the [`WATERFALL_DB_SPAN`] scale; positive brightens.
    gain_db: f32,
    /// Audio frequency range covered by the data bins (Hz)
    freq_range: (f64, f64),
    /// TX frequency offset (Hz) — shown as a vertical marker styled by the
    /// column's occupancy
    tx_offset: Option<f64>,
    /// Recent decoded signal frequencies (Hz) — shown as tick marks
    signal_freqs: Vec<f64>,
    /// Number of data rows per FT8 cycle (for drawing cycle boundary markers)
    rows_per_cycle: usize,
    /// Number of data rows combined into each display row (vertical compression)
    compression: usize,
    /// Terminal color capability — controls waterfall color palette
    color_capability: ColorCapability,
    /// Recent decodes (frequency, parity, timestamp) for the occupancy strip.
    decoded_for_occupancy: &'a [(
        f64,
        pancetta_core::slot::SlotParity,
        chrono::DateTime<chrono::Utc>,
    )],
    /// Operator's resolved TX parity (None when Auto + idle).
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Theme tokens for the panel fill, strip, axis, markers and TX cursor.
    theme: &'a Theme,
}

/// The dB span the waterfall's 0..1 colour scale covers (PAN-389).
///
/// Must equal `NOISE_FLOOR_DB_SCALE` in `pancetta/src/coordinator/dsp.rs`,
/// which normalises the live rows (signal above the per-bin median over this
/// many dB); a test there pins the two together so "6 dB = 0.5" cannot drift.
pub const WATERFALL_DB_SPAN: f32 = 12.0;

impl<'a> Waterfall<'a> {
    pub fn new(data: &'a [Vec<f32>]) -> Self {
        Self {
            block: None,
            data,
            palette: WaterfallPalette::Classic,
            gain_db: 0.0,
            freq_range: (0.0, 3000.0),
            tx_offset: None,
            signal_freqs: Vec::new(),
            rows_per_cycle: 15,
            compression: 2,
            color_capability: ColorCapability::TwoFiftySix,
            decoded_for_occupancy: &[],
            tx_parity: None,
            theme: &Theme::DARK,
        }
    }

    pub fn theme(mut self, theme: &'a Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set the colour palette (PAN-389).
    pub fn palette(mut self, palette: WaterfallPalette) -> Self {
        self.palette = palette;
        self
    }

    /// Set the display gain in dB (PAN-389). Positive brightens.
    pub fn gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn tx_offset(mut self, offset: f64) -> Self {
        self.tx_offset = Some(offset);
        self
    }

    pub fn signal_freqs(mut self, freqs: Vec<f64>) -> Self {
        self.signal_freqs = freqs;
        self
    }

    pub fn color_capability(mut self, cap: ColorCapability) -> Self {
        self.color_capability = cap;
        self
    }

    pub fn decoded_for_occupancy(
        mut self,
        decoded: &'a [(
            f64,
            pancetta_core::slot::SlotParity,
            chrono::DateTime<chrono::Utc>,
        )],
    ) -> Self {
        self.decoded_for_occupancy = decoded;
        self
    }

    pub fn tx_parity(mut self, parity: Option<pancetta_core::slot::SlotParity>) -> Self {
        self.tx_parity = parity;
        self
    }

    /// Map an audio frequency (Hz) to a column index in the waterfall area
    fn freq_to_col(&self, freq_hz: f64, width: usize) -> Option<usize> {
        let (lo, hi) = self.freq_range;
        if freq_hz < lo || freq_hz > hi || width == 0 {
            return None;
        }
        let frac = (freq_hz - lo) / (hi - lo);
        let col = (frac * width as f64) as usize;
        if col < width {
            Some(col)
        } else {
            None
        }
    }

    /// Choose a glyph for the given intensity in Basic-color terminals
    /// (xterm-style 16-color, often the case over SSH where COLORTERM and
    /// the 256color TERM suffix don't propagate). The previous palette
    /// painted the lowest band black, which is identical to the
    /// dark-theme background — the entire waterfall rendered invisible
    /// over SSH/tmux even though the data was flowing. Encoding intensity
    /// in the GLYPH (density blocks: ░ ▒ ▓ █) instead of (only) the color
    /// guarantees the user sees signal vs. silence on any 16-color terminal.
    fn glyph_for_intensity_basic(intensity: f32) -> char {
        let c = intensity.clamp(0.0, 1.0);
        if c < 0.10 {
            ' '
        } else if c < 0.30 {
            '░'
        } else if c < 0.55 {
            '▒'
        } else if c < 0.80 {
            '▓'
        } else {
            '█'
        }
    }
}

impl<'a> ratatui::widgets::Widget for Waterfall<'a> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let waterfall_area = match &self.block {
            Some(b) => {
                let inner = b.inner(area);
                b.clone().render(area, buf);
                inner
            }
            None => area,
        };

        if waterfall_area.width == 0 || waterfall_area.height == 0 {
            return;
        }

        // The spectrogram is a dark data panel in every theme.
        buf.set_style(waterfall_area, self.theme.waterfall_data);

        let width = waterfall_area.width as usize;
        let height = waterfall_area.height as usize;

        // Render waterfall data rows with vertical compression.
        // Each display row combines `compression` data rows (max intensity).
        let comp = self.compression.max(1);
        let data_len = self.data.len();
        if data_len >= comp {
            let effective_rows = data_len / comp;
            // Reserve top row for occupancy strip and bottom row for freq axis.
            let data_rows_available = height.saturating_sub(2);
            let rows_to_show = data_rows_available.min(effective_rows);

            for display_row in 0..rows_to_show {
                let y = waterfall_area.y + 1 + display_row as u16;

                // Source data rows for this display row (newest first)
                let end = data_len - display_row * comp;
                let start = end.saturating_sub(comp);
                let source_rows = &self.data[start..end];

                // Use the minimum bin count across compressed rows for safety
                let num_bins = source_rows.iter().map(|r| r.len()).min().unwrap_or(0);
                if num_bins == 0 {
                    continue;
                }

                for col in 0..width {
                    let bin_start = col * num_bins / width;
                    let bin_end = ((col + 1) * num_bins / width)
                        .max(bin_start + 1)
                        .min(num_bins);

                    // Max intensity across both the bin range and all compressed rows
                    let mut intensity = 0.0f32;
                    for row_data in source_rows {
                        for &val in &row_data[bin_start..bin_end] {
                            intensity = intensity.max(val);
                        }
                    }

                    // PAN-389: display gain, applied once per output cell
                    // (equivalent to per-sample because the map is monotone).
                    let shown = (intensity + self.gain_db / WATERFALL_DB_SPAN).clamp(0.0, 1.0);
                    let color = theme::waterfall_ramp(self.palette, self.color_capability, shown);
                    let glyph = if self.color_capability == ColorCapability::Basic {
                        Self::glyph_for_intensity_basic(shown)
                    } else {
                        '█'
                    };
                    let x = waterfall_area.x + col as u16;
                    buf[(x, y)].set_char(glyph).set_fg(color);
                }
            }
        }

        // Compute `now` once per render for the occupancy strip + cursor style.
        let now = chrono::Utc::now();

        // Occupancy strip: top row of the waterfall.
        for col in 0..width {
            let x = waterfall_area.x + col as u16;
            let strip_y = waterfall_area.y;
            match occupancy(
                col,
                width,
                self.freq_range,
                self.decoded_for_occupancy,
                self.tx_parity,
                now,
            ) {
                Occupancy::Clear => buf[(x, strip_y)].set_char('·').set_style(self.theme.muted),
                busy => buf[(x, strip_y)]
                    .set_char('█')
                    .set_style(self.theme.occupancy(busy)),
            };
        }

        // Frequency axis: bottom row.
        const TICK_FREQS: &[f64] = &[500.0, 1000.0, 1500.0, 2000.0, 2500.0];
        let axis_y = waterfall_area.y + waterfall_area.height - 1;
        for col in 0..width {
            let x = waterfall_area.x + col as u16;
            buf[(x, axis_y)].set_char('─').set_style(self.theme.muted);
        }
        for &freq in TICK_FREQS {
            if let Some(col) = self.freq_to_col(freq, width) {
                let x = waterfall_area.x + col as u16;
                buf[(x, axis_y)]
                    .set_char('┴')
                    .set_style(self.theme.secondary);
                if waterfall_area.width >= 40 && axis_y > waterfall_area.y + 1 {
                    let label = format!("{:.0}", freq);
                    let label_x = x.saturating_sub((label.len() / 2) as u16);
                    for (i, ch) in label.chars().enumerate() {
                        let lx = label_x + i as u16;
                        if lx < waterfall_area.x + waterfall_area.width {
                            buf[(lx, axis_y - 1)]
                                .set_char(ch)
                                .set_style(self.theme.muted);
                        }
                    }
                }
            }
        }

        // Overlay: TX frequency marker (parity-aware vertical line). The
        // suffix follows the occupancy state, never the colour, so it stays
        // right for a theme that paints two states alike.
        if let Some(tx_hz) = self.tx_offset {
            if let Some(col) = self.freq_to_col(tx_hz, width) {
                let x = waterfall_area.x + col as u16;
                let occ = occupancy(
                    col,
                    width,
                    self.freq_range,
                    self.decoded_for_occupancy,
                    self.tx_parity,
                    now,
                );
                let cursor_style = self.theme.waterfall_data.patch(self.theme.occupancy(occ));
                let suffix = match occ {
                    Occupancy::OwnParity => " ✗",
                    Occupancy::OtherParity => " ⚠",
                    Occupancy::Clear => "",
                };
                for row in 0..height {
                    let y = waterfall_area.y + row as u16;
                    buf[(x, y)].set_char('│').set_style(cursor_style);
                }
                let label = format!("TX {:.0}{}", tx_hz, suffix);
                for (i, ch) in label.chars().enumerate() {
                    let lx = x.saturating_add(1) + i as u16;
                    if lx < waterfall_area.x + waterfall_area.width {
                        buf[(lx, waterfall_area.y)]
                            .set_char(ch)
                            .set_style(cursor_style);
                    }
                }
            }
        }

        // Overlay: decoded signal markers (ticks on the top two rows)
        for &freq_hz in &self.signal_freqs {
            if let Some(col) = self.freq_to_col(freq_hz, width) {
                let x = waterfall_area.x + col as u16;
                // Draw a short tick (top 2 rows)
                for row in 0..2.min(height) {
                    let y = waterfall_area.y + row as u16;
                    buf[(x, y)].set_char('▼').set_style(self.theme.decode_tick);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_own_parity_when_decode_in_own_parity() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
        let c = occupancy(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        assert_eq!(c, Occupancy::OwnParity);
    }

    #[test]
    fn occupancy_other_parity_when_decode_in_other_parity_only() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Odd, now - Duration::seconds(10))];
        let c = occupancy(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        assert_eq!(c, Occupancy::OtherParity);
    }

    #[test]
    fn occupancy_drops_decodes_older_than_60s() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(120))];
        let c = occupancy(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        // 60+s old decode should be excluded; column has no relevant decodes.
        // With an empty band-overall (all decodes filtered), returns Clear.
        assert_eq!(c, Occupancy::Clear);
    }

    #[test]
    fn occupancy_drops_decodes_outside_band() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(2000.0, SlotParity::Even, now - Duration::seconds(10))];
        // Column 40 in width 80 over (0,3000) = 1500 Hz ± 18.75. 2000 is well outside.
        let c = occupancy(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        // Decode exists but not in this column's band — column itself has
        // nothing nearby in either parity → Clear.
        assert_eq!(c, Occupancy::Clear);
    }

    #[test]
    fn waterfall_renders_occupancy_strip_top_row() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let now = Utc::now();
        let data: Vec<Vec<f32>> = vec![vec![0.5; 64]; 30];
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(5))];
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);

        Waterfall::new(&data)
            .decoded_for_occupancy(&decoded)
            .tx_parity(Some(SlotParity::Even))
            .render(area, &mut buf);

        // The column for 1500 Hz over (0,3000) and width 80 is column 40.
        // Top row should show '█' in the own-parity style because the
        // decode is in our parity.
        let cell = &buf[(40, 0)];
        assert_eq!(cell.symbol(), "█");
        assert_eq!(cell.fg, theme::fg_color(Theme::DARK.occupancy_own));
    }

    #[test]
    fn waterfall_renders_freq_axis_bottom_row() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let data: Vec<Vec<f32>> = vec![vec![0.0; 64]; 10];
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);

        Waterfall::new(&data).render(area, &mut buf);

        // 1500 Hz tick at column 40 in the bottom row.
        let tick = &buf[(40, 9)];
        assert_eq!(tick.symbol(), "┴");
    }

    #[test]
    fn occupancy_other_parity_when_no_tx_parity_known() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
        // tx_parity = None means we can't say "your" vs "their" — collapse
        // to OtherParity.
        let c = occupancy(40, 80, (0.0, 3000.0), &decoded, None, now);
        assert_eq!(c, Occupancy::OtherParity);
    }

    #[test]
    fn cursor_own_parity_when_offset_overlaps_busy_own_parity() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let now = Utc::now();
        let data: Vec<Vec<f32>> = vec![vec![0.0; 64]; 10];
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(5))];
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);

        Waterfall::new(&data)
            .tx_offset(1500.0)
            .decoded_for_occupancy(&decoded)
            .tx_parity(Some(SlotParity::Even))
            .render(area, &mut buf);

        // Cursor at col 40, mid-height, in the own-parity style because the
        // column is busy in our parity.
        let cell = &buf[(40, 5)];
        assert_eq!(cell.symbol(), "│");
        assert_eq!(cell.fg, theme::fg_color(Theme::DARK.occupancy_own));
        assert_eq!(cell.bg, theme::bg_color(Theme::DARK.waterfall_data));
    }

    /// AC9: the TX-cursor suffix follows the occupancy state, not the
    /// colour. A theme that paints own- and other-parity alike must still
    /// label the cursor ` ✗` for own parity and ` ⚠` for other parity.
    #[test]
    fn tx_cursor_suffix_follows_occupancy_not_colour() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let alike = Theme {
            occupancy_own: Theme::DARK.occupancy_other,
            ..Theme::DARK
        };
        let now = Utc::now();
        let data: Vec<Vec<f32>> = vec![vec![0.0; 64]; 10];
        let area = Rect::new(0, 0, 80, 10);

        let render = |decode_parity: SlotParity| {
            let decoded = vec![(1500.0, decode_parity, now - Duration::seconds(5))];
            let mut buf = Buffer::empty(area);
            Waterfall::new(&data)
                .theme(&alike)
                .tx_offset(1500.0)
                .decoded_for_occupancy(&decoded)
                .tx_parity(Some(SlotParity::Even))
                .render(area, &mut buf);
            // The cursor sits at column 40; its label starts one column
            // to the right on the top row.
            let label: String = (41..50).map(|x| buf[(x, 0)].symbol()).collect();
            (label, buf[(40, 5)].fg)
        };

        let (own_label, own_fg) = render(SlotParity::Even);
        let (other_label, other_fg) = render(SlotParity::Odd);

        assert_eq!(own_label, "TX 1500 ✗");
        assert_eq!(other_label, "TX 1500 ⚠");
        // Both states paint the same colour under this theme, so only the
        // enum can have told the suffixes apart.
        assert_eq!(own_fg, other_fg);
        assert_eq!(own_fg, theme::fg_color(alike.occupancy_other));
    }
}

#[cfg(test)]
#[path = "tests/waterfall_palette_gain_tests.rs"]
mod waterfall_palette_gain_tests;
