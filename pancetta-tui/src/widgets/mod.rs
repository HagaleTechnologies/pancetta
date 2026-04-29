use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::app::ColorCapability;

/// Color a column of the occupancy strip by recent decode activity.
/// Red = decode within ±37.5 Hz in YOUR TX parity in the last 60s.
/// Yellow = decode within ±37.5 Hz in the OTHER parity (or own-parity unknown).
/// None = column is clear (no decodes nearby in the last 60s).
///
/// `decoded` is `(frequency_hz, parity, timestamp)`. `tx_parity = None`
/// (operator's parity unknown — Auto + idle) collapses red→yellow because
/// we can't say if a decode would collide.
pub(crate) fn occupancy_color(
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
) -> Option<ratatui::style::Color> {
    if width == 0 {
        return None;
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
        return None;
    }

    match tx_parity {
        Some(my_parity) => {
            let busy_own = decoded.iter().any(|d| in_band(d) && d.1 == my_parity);
            if busy_own {
                Some(ratatui::style::Color::Red)
            } else {
                Some(ratatui::style::Color::Yellow)
            }
        }
        None => Some(ratatui::style::Color::Yellow),
    }
}

/// Custom waterfall widget for displaying spectrum data
pub struct Waterfall<'a> {
    block: Option<Block<'a>>,
    data: &'a [Vec<f32>],
    color_scheme: WaterfallColorScheme,
    /// Audio frequency range covered by the data bins (Hz)
    freq_range: (f64, f64),
    /// TX frequency offset (Hz) — shown as a green vertical marker
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
}

#[derive(Clone, Copy)]
pub enum WaterfallColorScheme {
    Classic,
    Spectrum,
    Thermal,
}

impl<'a> Waterfall<'a> {
    pub fn new(data: &'a [Vec<f32>]) -> Self {
        Self {
            block: None,
            data,
            color_scheme: WaterfallColorScheme::Classic,
            freq_range: (0.0, 3000.0),
            tx_offset: None,
            signal_freqs: Vec::new(),
            rows_per_cycle: 15,
            compression: 2,
            color_capability: ColorCapability::TwoFiftySix,
            decoded_for_occupancy: &[],
            tx_parity: None,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn color_scheme(mut self, scheme: WaterfallColorScheme) -> Self {
        self.color_scheme = scheme;
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
    /// returned `Color::Black` for the lowest band, which is identical to
    /// the dark-theme background — the entire waterfall rendered invisible
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

    fn get_color_for_intensity(&self, intensity: f32) -> Color {
        let clamped = intensity.clamp(0.0, 1.0);

        // 16-color fallback: pair density glyphs (above) with a bright,
        // always-visible foreground. Don't return Color::Black here — the
        // glyph is what encodes intensity now, and we need the few cells
        // that DO render (anything above 10%) to be unambiguously visible.
        if self.color_capability == ColorCapability::Basic {
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

        match self.color_scheme {
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

                    let color = self.get_color_for_intensity(intensity);
                    let glyph = if self.color_capability == ColorCapability::Basic {
                        Self::glyph_for_intensity_basic(intensity)
                    } else {
                        '█'
                    };
                    let x = waterfall_area.x + col as u16;
                    buf[(x, y)].set_char(glyph).set_fg(color);
                }
            }
        }

        // Compute `now` once per render for the occupancy strip + cursor color.
        let now = chrono::Utc::now();

        // Occupancy strip: top row of the waterfall.
        for col in 0..width {
            let x = waterfall_area.x + col as u16;
            let strip_y = waterfall_area.y;
            if let Some(color) = occupancy_color(
                col,
                width,
                self.freq_range,
                self.decoded_for_occupancy,
                self.tx_parity,
                now,
            ) {
                buf[(x, strip_y)].set_char('█').set_fg(color);
            } else {
                buf[(x, strip_y)]
                    .set_char('·')
                    .set_fg(ratatui::style::Color::DarkGray);
            }
        }

        // Frequency axis: bottom row.
        const TICK_FREQS: &[f64] = &[500.0, 1000.0, 1500.0, 2000.0, 2500.0];
        let axis_y = waterfall_area.y + waterfall_area.height - 1;
        for col in 0..width {
            let x = waterfall_area.x + col as u16;
            buf[(x, axis_y)]
                .set_char('─')
                .set_fg(ratatui::style::Color::DarkGray);
        }
        for &freq in TICK_FREQS {
            if let Some(col) = self.freq_to_col(freq, width) {
                let x = waterfall_area.x + col as u16;
                buf[(x, axis_y)]
                    .set_char('┴')
                    .set_fg(ratatui::style::Color::Gray);
                if waterfall_area.width >= 40 && axis_y > waterfall_area.y + 1 {
                    let label = format!("{:.0}", freq);
                    let label_x = x.saturating_sub((label.len() / 2) as u16);
                    for (i, ch) in label.chars().enumerate() {
                        let lx = label_x + i as u16;
                        if lx < waterfall_area.x + waterfall_area.width {
                            buf[(lx, axis_y - 1)]
                                .set_char(ch)
                                .set_fg(ratatui::style::Color::DarkGray);
                        }
                    }
                }
            }
        }

        // Overlay: cycle boundary markers (dim horizontal ticks on left edge)
        // Every rows_per_cycle data rows, draw a small marker so the operator
        // can distinguish even/odd FT8 cycles.  Accounts for compression.
        if self.rows_per_cycle > 0 && data_len >= comp {
            let effective_rows = data_len / comp;
            let rows_to_show = height.min(effective_rows);
            for display_row in 0..rows_to_show {
                // Map display row back to data index
                let data_idx = data_len - 1 - display_row * comp;
                if data_idx.is_multiple_of(self.rows_per_cycle) {
                    let y = waterfall_area.y + display_row as u16;
                    let cycle_num = data_idx / self.rows_per_cycle;
                    let marker_char = if cycle_num.is_multiple_of(2) {
                        'E'
                    } else {
                        'O'
                    };
                    let marker_color = if cycle_num.is_multiple_of(2) {
                        Color::DarkGray
                    } else {
                        Color::Gray
                    };
                    buf[(waterfall_area.x, y)]
                        .set_char(marker_char)
                        .set_fg(marker_color);
                }
            }
        }

        // Overlay: TX frequency marker (green vertical line)
        if let Some(tx_hz) = self.tx_offset {
            if let Some(col) = self.freq_to_col(tx_hz, width) {
                let x = waterfall_area.x + col as u16;
                for row in 0..height {
                    let y = waterfall_area.y + row as u16;
                    buf[(x, y)]
                        .set_char('│')
                        .set_fg(Color::Green)
                        .set_bg(Color::Black);
                }
                // Label at top
                let label = format!("TX {:.0}", tx_hz);
                for (i, ch) in label.chars().enumerate() {
                    let lx = x.saturating_add(1) + i as u16;
                    if lx < waterfall_area.x + waterfall_area.width {
                        buf[(lx, waterfall_area.y)]
                            .set_char(ch)
                            .set_fg(Color::Green)
                            .set_bg(Color::Black);
                    }
                }
            }
        }

        // Overlay: decoded signal markers (yellow ticks on the top row)
        for &freq_hz in &self.signal_freqs {
            if let Some(col) = self.freq_to_col(freq_hz, width) {
                let x = waterfall_area.x + col as u16;
                // Draw a short tick (top 2 rows)
                for row in 0..2.min(height) {
                    let y = waterfall_area.y + row as u16;
                    buf[(x, y)].set_char('▼').set_fg(Color::Yellow);
                }
            }
        }
    }
}

/// Custom signal strength meter widget
pub struct SignalMeter<'a> {
    block: Option<Block<'a>>,
    level: f32,
    label: Option<&'a str>,
    style: Style,
    threshold_low: f32,
    threshold_high: f32,
}

impl<'a> SignalMeter<'a> {
    pub fn new(level: f32) -> Self {
        Self {
            block: None,
            level: level.clamp(0.0, 1.0),
            label: None,
            style: Style::default(),
            threshold_low: 0.3,
            threshold_high: 0.8,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn thresholds(mut self, low: f32, high: f32) -> Self {
        self.threshold_low = low;
        self.threshold_high = high;
        self
    }

    fn get_meter_color(&self) -> Color {
        if self.level < self.threshold_low {
            Color::Red
        } else if self.level < self.threshold_high {
            Color::Yellow
        } else {
            Color::Green
        }
    }
}

impl<'a> ratatui::widgets::Widget for SignalMeter<'a> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let meter_area = match &self.block {
            Some(b) => {
                let inner = b.inner(area);
                b.clone().render(area, buf);
                inner
            }
            None => area,
        };

        if meter_area.width == 0 || meter_area.height == 0 {
            return;
        }

        // Calculate filled width
        let filled_width = ((self.level * meter_area.width as f32) as u16).min(meter_area.width);
        let color = self.get_meter_color();

        // Render meter bars
        for y in meter_area.y..meter_area.y + meter_area.height {
            for x in meter_area.x..meter_area.x + meter_area.width {
                let char = if x < meter_area.x + filled_width {
                    '█'
                } else {
                    '░'
                };

                let fg_color = if x < meter_area.x + filled_width {
                    color
                } else {
                    Color::DarkGray
                };

                buf[(x, y)].set_char(char).set_fg(fg_color);
            }
        }

        // Render label if provided
        if let Some(label) = self.label {
            let label_x = meter_area.x + (meter_area.width.saturating_sub(label.len() as u16)) / 2;
            let label_y = meter_area.y + meter_area.height / 2;

            if label_y < meter_area.y + meter_area.height {
                for (i, ch) in label.chars().enumerate() {
                    let x = label_x + i as u16;
                    if x < meter_area.x + meter_area.width {
                        buf[(x, label_y)]
                            .set_char(ch)
                            .set_fg(Color::Black)
                            .set_bg(color);
                    }
                }
            }
        }
    }
}

/// Modal dialog widget for user input or confirmation
pub struct Modal<'a> {
    title: &'a str,
    content: Vec<Line<'a>>,
    buttons: Vec<&'a str>,
    selected_button: usize,
    style: Style,
}

impl<'a> Modal<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            content: Vec::new(),
            buttons: vec!["OK"],
            selected_button: 0,
            style: Style::default(),
        }
    }

    pub fn content(mut self, content: Vec<Line<'a>>) -> Self {
        self.content = content;
        self
    }

    pub fn buttons(mut self, buttons: Vec<&'a str>) -> Self {
        self.buttons = buttons;
        self
    }

    pub fn selected_button(mut self, index: usize) -> Self {
        self.selected_button = index.min(self.buttons.len().saturating_sub(1));
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> ratatui::widgets::Widget for Modal<'a> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        // Calculate modal size (centered, with padding)
        let modal_width = (area.width * 2 / 3).min(60);
        let modal_height = (self.content.len() + 5) as u16; // Content + title + buttons + padding

        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        // Clear the background
        Clear.render(modal_area, buf);

        // Create the modal block
        let block = Block::default()
            .title(self.title)
            .borders(Borders::ALL)
            .style(self.style);

        let inner_area = block.inner(modal_area);
        block.render(modal_area, buf);

        // Split inner area for content and buttons
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Content
                Constraint::Length(1), // Spacing
                Constraint::Length(1), // Buttons
            ])
            .split(inner_area);

        // Render content
        if !self.content.is_empty() {
            let content_paragraph = Paragraph::new(self.content);
            content_paragraph.render(chunks[0], buf);
        }

        // Render buttons
        if !self.buttons.is_empty() {
            let button_text: Vec<Span> = self
                .buttons
                .iter()
                .enumerate()
                .flat_map(|(i, &button)| {
                    let style = if i == self.selected_button {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };

                    let mut spans = vec![Span::styled(format!(" {} ", button), style)];
                    if i < self.buttons.len() - 1 {
                        spans.push(Span::raw("  "));
                    }
                    spans
                })
                .collect();

            let buttons_line = Line::from(button_text);
            let buttons_paragraph = Paragraph::new(buttons_line);
            buttons_paragraph.render(chunks[2], buf);
        }
    }
}

/// Frequency spectrum display widget
pub struct SpectrumDisplay<'a> {
    block: Option<Block<'a>>,
    data: &'a [f32],
    frequency_range: (f64, f64),
    style: Style,
    show_grid: bool,
}

impl<'a> SpectrumDisplay<'a> {
    pub fn new(data: &'a [f32], frequency_range: (f64, f64)) -> Self {
        Self {
            block: None,
            data,
            frequency_range,
            style: Style::default(),
            show_grid: true,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn show_grid(mut self, show: bool) -> Self {
        self.show_grid = show;
        self
    }
}

impl<'a> ratatui::widgets::Widget for SpectrumDisplay<'a> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let spectrum_area = match &self.block {
            Some(b) => {
                let inner = b.inner(area);
                b.clone().render(area, buf);
                inner
            }
            None => area,
        };

        if self.data.is_empty() || spectrum_area.width == 0 || spectrum_area.height == 0 {
            return;
        }

        // Find min/max for scaling
        let min_val = self.data.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        let max_val = self.data.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        let range = max_val - min_val;

        if range == 0.0 {
            return;
        }

        // Draw spectrum
        let bins_per_pixel = self.data.len() as f32 / spectrum_area.width as f32;

        for x in 0..spectrum_area.width {
            let bin_index = (x as f32 * bins_per_pixel) as usize;
            if bin_index >= self.data.len() {
                break;
            }

            let normalized = (self.data[bin_index] - min_val) / range;
            let bar_height = (normalized * spectrum_area.height as f32) as u16;

            for y in 0..bar_height.min(spectrum_area.height) {
                let actual_y = spectrum_area.y + spectrum_area.height - 1 - y;
                let actual_x = spectrum_area.x + x;

                // Color based on intensity
                let color = if normalized > 0.8 {
                    Color::Red
                } else if normalized > 0.6 {
                    Color::Yellow
                } else if normalized > 0.4 {
                    Color::Green
                } else {
                    Color::Cyan
                };

                buf[(actual_x, actual_y)].set_char('█').set_fg(color);
            }
        }

        // Draw grid if enabled
        if self.show_grid {
            // Horizontal grid lines
            for i in 1..4 {
                let y = spectrum_area.y + (spectrum_area.height * i) / 4;
                for x in spectrum_area.x..spectrum_area.x + spectrum_area.width {
                    buf[(x, y)].set_char('─').set_fg(Color::DarkGray);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_waterfall_color_intensity() {
        let waterfall = Waterfall::new(&[]).color_scheme(WaterfallColorScheme::Classic);

        assert!(matches!(
            waterfall.get_color_for_intensity(0.1),
            Color::Indexed(_)
        ));
        assert!(matches!(
            waterfall.get_color_for_intensity(0.5),
            Color::Indexed(_)
        ));
        assert!(matches!(
            waterfall.get_color_for_intensity(0.9),
            Color::Indexed(_)
        ));
    }

    #[test]
    fn test_signal_meter_color_thresholds() {
        let meter = SignalMeter::new(0.5).thresholds(0.3, 0.8);
        assert_eq!(meter.get_meter_color(), Color::Yellow);

        let meter_low = SignalMeter::new(0.1).thresholds(0.3, 0.8);
        assert_eq!(meter_low.get_meter_color(), Color::Red);

        let meter_high = SignalMeter::new(0.9).thresholds(0.3, 0.8);
        assert_eq!(meter_high.get_meter_color(), Color::Green);
    }

    #[test]
    fn occupancy_red_when_decode_in_own_parity() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
        let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        assert_eq!(c, Some(Color::Red));
    }

    #[test]
    fn occupancy_yellow_when_decode_in_other_parity_only() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Odd, now - Duration::seconds(10))];
        let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        assert_eq!(c, Some(Color::Yellow));
    }

    #[test]
    fn occupancy_drops_decodes_older_than_60s() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(120))];
        let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        // 60+s old decode should be excluded; column has no relevant decodes.
        // With an empty band-overall (all decodes filtered), returns None.
        assert_eq!(c, None);
    }

    #[test]
    fn occupancy_drops_decodes_outside_band() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(2000.0, SlotParity::Even, now - Duration::seconds(10))];
        // Column 40 in width 80 over (0,3000) = 1500 Hz ± 18.75. 2000 is well outside.
        let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
        // Decode exists but not in this column's band — column itself has
        // nothing nearby in either parity → None.
        assert_eq!(c, None);
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
        // Top row should show '█' colored Red because decode is in our parity.
        let cell = &buf[(40, 0)];
        assert_eq!(cell.symbol(), "█");
        assert_eq!(cell.fg, ratatui::style::Color::Red);
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
    fn occupancy_yellow_when_no_tx_parity_known() {
        use chrono::{Duration, Utc};
        use pancetta_core::slot::SlotParity;
        let now = Utc::now();
        let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
        // tx_parity = None means we can't say "your" vs "their" — collapse to yellow.
        let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, None, now);
        assert_eq!(c, Some(Color::Yellow));
    }
}
