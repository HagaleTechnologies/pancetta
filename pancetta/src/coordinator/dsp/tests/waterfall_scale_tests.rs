// PAN-389. Formerly an inline module of the same name in `dsp.rs`.
use super::NOISE_FLOOR_DB_SCALE;

/// PAN-389 AC19: the TUI's "gain_db / 12 dB" display offset must match
/// the scale the live rows are normalised on, so 6 dB stays 0.5.
#[test]
fn waterfall_db_span_matches_the_dsp_noise_floor_scale() {
    assert_eq!(
        pancetta_tui::widgets::WATERFALL_DB_SPAN,
        NOISE_FLOOR_DB_SCALE
    );
}
