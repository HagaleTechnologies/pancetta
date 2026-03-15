use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Custom waterfall widget for displaying spectrum data
pub struct Waterfall<'a> {
    block: Option<Block<'a>>,
    data: &'a [Vec<f32>],
    color_scheme: WaterfallColorScheme,
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

    fn get_color_for_intensity(&self, intensity: f32) -> Color {
        let clamped = intensity.clamp(0.0, 1.0);

        match self.color_scheme {
            WaterfallColorScheme::Classic => {
                if clamped < 0.2 {
                    Color::Blue
                } else if clamped < 0.4 {
                    Color::Cyan
                } else if clamped < 0.6 {
                    Color::Green
                } else if clamped < 0.8 {
                    Color::Yellow
                } else {
                    Color::Red
                }
            }
            WaterfallColorScheme::Spectrum => {
                // Rainbow spectrum
                let hue = (1.0 - clamped) * 240.0; // Blue to Red
                Color::Indexed(((hue / 360.0) * 255.0) as u8)
            }
            WaterfallColorScheme::Thermal => {
                if clamped < 0.33 {
                    Color::Black
                } else if clamped < 0.66 {
                    Color::Red
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

        if self.data.is_empty() || waterfall_area.width == 0 || waterfall_area.height == 0 {
            return;
        }

        // Render waterfall data
        let rows_to_show = (waterfall_area.height as usize).min(self.data.len());
        let cols_per_bin = if self.data[0].is_empty() {
            1
        } else {
            (waterfall_area.width as usize) / self.data[0].len().max(1)
        };

        for (row_idx, row_data) in self
            .data
            .iter()
            .rev() // Newest at top
            .take(rows_to_show)
            .enumerate()
        {
            let y = waterfall_area.y + row_idx as u16;

            for (bin_idx, &intensity) in row_data.iter().enumerate() {
                let x_start = waterfall_area.x + (bin_idx * cols_per_bin.max(1)) as u16;
                let x_end =
                    (x_start + cols_per_bin as u16).min(waterfall_area.x + waterfall_area.width);

                if x_start >= waterfall_area.x + waterfall_area.width {
                    break;
                }

                let color = self.get_color_for_intensity(intensity);

                for x in x_start..x_end {
                    if x < waterfall_area.x + waterfall_area.width {
                        buf[(x, y)].set_char('█').set_fg(color);
                    }
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

        assert_eq!(waterfall.get_color_for_intensity(0.1), Color::Blue);
        assert_eq!(waterfall.get_color_for_intensity(0.5), Color::Green);
        assert_eq!(waterfall.get_color_for_intensity(0.9), Color::Red);
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
}
