// PAN-389. Waterfall palette and gain in the widget: the gain offsets the
// intensity fed to the palette ramp, viridis maps to its ramp, the default
// render is unchanged, and a 16-colour terminal applies gain only.
use super::*;

use ratatui::style::Color;

/// Render a waterfall of uniform `value` rows and return the first data
/// cell (row 1; row 0 is the occupancy strip).
fn data_cell(
    value: f32,
    palette: Option<WaterfallPalette>,
    gain_db: Option<f32>,
    capability: ColorCapability,
) -> (String, Color) {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let data: Vec<Vec<f32>> = vec![vec![value; 16]; 4];
    let area = Rect::new(0, 0, 16, 6);
    let mut buf = Buffer::empty(area);
    let mut w = Waterfall::new(&data).color_capability(capability);
    if let Some(p) = palette {
        w = w.palette(p);
    }
    if let Some(g) = gain_db {
        w = w.gain_db(g);
    }
    w.render(area, &mut buf);
    let cell = &buf[(3, 1)];
    (cell.symbol().to_string(), cell.fg)
}

fn fg(value: f32, palette: Option<WaterfallPalette>, gain_db: Option<f32>) -> Color {
    data_cell(value, palette, gain_db, ColorCapability::TwoFiftySix).1
}

/// The 256-colour ramp entry for `palette` at `intensity`. The colour
/// values live in theme.rs (PAN-387) and are pinned there; these tests
/// check which intensity the widget feeds the ramp.
fn ramp(palette: WaterfallPalette, intensity: f32) -> Color {
    theme::waterfall_ramp(palette, ColorCapability::TwoFiftySix, intensity)
}

#[test]
fn default_render_is_classic_with_zero_gain() {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let data: Vec<Vec<f32>> = (0..8)
        .map(|r| (0..32).map(|b| ((r * 32 + b) % 11) as f32 / 10.0).collect())
        .collect();
    let area = Rect::new(0, 0, 32, 8);
    let mut default_buf = Buffer::empty(area);
    Waterfall::new(&data).render(area, &mut default_buf);
    let mut explicit_buf = Buffer::empty(area);
    Waterfall::new(&data)
        .palette(WaterfallPalette::Classic)
        .gain_db(0.0)
        .render(area, &mut explicit_buf);
    assert_eq!(default_buf, explicit_buf);

    for i in 0..=10 {
        let v = i as f32 / 10.0;
        assert_eq!(
            fg(v, None, None),
            ramp(WaterfallPalette::Classic, v),
            "intensity {v}"
        );
    }
}

#[test]
fn gain_raises_the_floor_by_gain_over_twelve_db() {
    use WaterfallPalette::Classic;
    let classic = Some(Classic);
    assert_ne!(ramp(Classic, 0.0), ramp(Classic, 0.5));
    assert_eq!(fg(0.0, classic, Some(6.0)), ramp(Classic, 0.5));
    assert_eq!(fg(0.5, classic, None), ramp(Classic, 0.5));
    assert_eq!(fg(0.25, classic, Some(6.0)), ramp(Classic, 0.75));
    assert_eq!(fg(0.75, classic, Some(6.0)), ramp(Classic, 1.0));
    assert_eq!(fg(0.5, classic, Some(-6.0)), ramp(Classic, 0.0));
}

#[test]
fn viridis_palette_maps_to_its_256_colour_ramp() {
    use WaterfallPalette::{Classic, Viridis};
    let viridis = Some(Viridis);
    for v in [0.0, 0.5, 1.0] {
        assert_eq!(fg(v, viridis, None), ramp(Viridis, v), "intensity {v}");
    }
    assert_ne!(ramp(Viridis, 0.5), ramp(Classic, 0.5));
    assert_eq!(fg(0.0, viridis, Some(6.0)), ramp(Viridis, 0.5));
}

#[test]
fn basic_terminal_applies_gain_and_ignores_palette() {
    let (glyph, color) = data_cell(0.0, None, Some(6.0), ColorCapability::Basic);
    assert_eq!(glyph, "▒");
    assert_eq!(
        color,
        theme::waterfall_ramp(WaterfallPalette::Classic, ColorCapability::Basic, 0.5)
    );
    for p in WaterfallPalette::ALL {
        for v in [0.0, 0.2, 0.5, 0.9] {
            assert_eq!(
                data_cell(v, Some(p), Some(6.0), ColorCapability::Basic),
                data_cell(v, None, Some(6.0), ColorCapability::Basic),
                "palette {p:?} at {v}"
            );
        }
    }
}
