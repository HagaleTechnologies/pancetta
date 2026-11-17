// PAN-389. Waterfall palette cycling and labels, and the gain label format.
use super::*;

#[test]
fn waterfall_palette_cycles_in_both_directions() {
    let mut p = WaterfallPalette::Classic;
    let mut forward = Vec::new();
    for _ in 0..4 {
        p = p.next();
        forward.push(p);
    }
    assert_eq!(
        forward,
        vec![
            WaterfallPalette::Spectrum,
            WaterfallPalette::Thermal,
            WaterfallPalette::Viridis,
            WaterfallPalette::Classic,
        ]
    );
    assert_eq!(WaterfallPalette::Classic.prev(), WaterfallPalette::Viridis);
    for p in WaterfallPalette::ALL {
        assert_eq!(p.next().prev(), p);
    }
}

#[test]
fn waterfall_palette_labels_round_trip() {
    for p in WaterfallPalette::ALL {
        assert_eq!(WaterfallPalette::from_label(p.as_str()), Some(p));
    }
    assert_eq!(WaterfallPalette::from_label("inferno"), None);
}

#[test]
fn format_gain_db_labels() {
    assert_eq!(format_gain_db(0.0), "0 dB");
    assert_eq!(format_gain_db(-0.0), "0 dB");
    assert_eq!(format_gain_db(6.0), "+6 dB");
    assert_eq!(format_gain_db(-3.0), "-3 dB");
    assert_eq!(format_gain_db(2.5), "+2.5 dB");
    assert_eq!(format_gain_db(12.0), "+12 dB");
}
