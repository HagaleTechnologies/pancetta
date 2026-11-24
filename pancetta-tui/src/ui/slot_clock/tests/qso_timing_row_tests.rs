//! PAN-477: the QSO panel's timing row follows the active mode's slot grid,
//! and FT8 keeps the historical whole-second row at every instant.

use super::*;

/// 2026-01-01 00:00:00 UTC, a boundary for FT8, FT4 and FT2.
fn at(ms: i64) -> UtcTimestamp {
    "2026-01-01T00:00:00Z".parse::<UtcTimestamp>().unwrap() + SignedDuration::from_millis(ms)
}

fn assert_ratio(actual: f64, expected: f64, context: &str) {
    assert!(
        (actual - expected).abs() < 1e-12,
        "{context}: ratio {actual} != {expected}"
    );
}

#[test]
fn ft8_row_matches_the_previous_whole_second_formula() {
    for ms in (0..30_000).step_by(100) {
        let now = at(ms);
        // The formula `qso_status.rs` used before PAN-477.
        let seconds_in_cycle = now.as_second() % 15;
        let old_label = if seconds_in_cycle < 13 {
            "RX Period"
        } else {
            "TX Window"
        };
        let old_ratio = seconds_in_cycle as f64 / 15.0;
        let old_countdown = 15 - seconds_in_cycle;

        let row = qso_timing_row(now, "FT8");
        assert_eq!(row.label, old_label, "label at {ms} ms");
        assert_eq!(row.ratio, old_ratio, "fill at {ms} ms");
        assert_eq!(row.remaining_secs, old_countdown, "countdown at {ms} ms");
    }
}

#[test]
fn ft4_and_ft2_rows_use_the_mode_period_and_decode_point() {
    // Scenario B: FT4 at 00:00:02.0, 2 s into a 7.5 s slot.
    let b = qso_timing_row(at(2_000), "FT4");
    assert_eq!(b.label, "RX Period");
    assert_ratio(b.ratio, 2.0 / 7.5, "B");
    assert_eq!(b.remaining_secs, 6);

    // Scenario C: FT4 at 00:00:07.0, past the 6.5 s decode point.
    let c = qso_timing_row(at(7_000), "FT4");
    assert_eq!(c.label, "TX Window");
    assert_ratio(c.ratio, 7.0 / 7.5, "C");
    assert_eq!(c.remaining_secs, 1);

    // Scenario D: FT2 at 00:00:05.6, 2.4 s into the slot starting 00:00:03.2.
    let d = qso_timing_row(at(5_600), "FT2");
    assert_eq!(d.label, "TX Window");
    assert_ratio(d.ratio, 2.0 / 3.2, "D");
    assert_eq!(d.remaining_secs, 1);
    assert_eq!(d.slot_start, at(3_200));
    assert_eq!(d.parity, SlotParity::Odd);

    // The label turns at each mode's decode point, not FT8's 13 s.
    assert_eq!(qso_timing_row(at(6_499), "FT4").label, "RX Period");
    assert_eq!(qso_timing_row(at(6_500), "FT4").label, "TX Window");
    assert_eq!(qso_timing_row(at(7_500), "FT4").label, "RX Period");
    assert_eq!(qso_timing_row(at(3_200 + 2_199), "FT2").label, "RX Period");
    assert_eq!(qso_timing_row(at(3_200 + 2_200), "FT2").label, "TX Window");

    // The fill never reaches 1 and the countdown never reaches 0.
    for mode in ["FT4", "FT2"] {
        let period_ms = slot_period_ns(mode) / 1_000_000;
        for ms in (0..period_ms * 3).step_by(50) {
            let row = qso_timing_row(at(ms), mode);
            assert!((0.0..1.0).contains(&row.ratio), "{mode} {ms} ms");
            assert!(row.remaining_secs >= 1, "{mode} {ms} ms");
        }
    }
}

#[test]
fn row_and_title_chip_agree_on_the_slot() {
    for mode in ["FT8", "FT4", "FT2"] {
        let period = slot_period_ns(mode);
        for ms in (0..period / 1_000_000 * 2).step_by(100) {
            let now = at(ms);
            let row = qso_timing_row(now, mode);
            assert_eq!(
                row.slot_start,
                current_slot_start_with_period(now, period),
                "{mode} {ms} ms"
            );
            let chip = slot_clock_text(now, mode, None);
            let fields: Vec<&str> = chip.split_whitespace().collect();
            let letter = match row.parity {
                SlotParity::Even => "[E",
                SlotParity::Odd => "[O",
            };
            assert_eq!(fields[0], letter, "{mode} {ms} ms: {chip}");
            let chip_secs: i64 = fields[2].trim_end_matches('s').parse().unwrap();
            let row_secs = (now - row.slot_start).as_nanos_i64().unwrap() / 1_000_000_000;
            assert_eq!(chip_secs, row_secs, "{mode} {ms} ms: {chip}");
        }
    }
}

#[test]
fn unknown_mode_uses_the_ft8_row() {
    for ms in [0, 6_500, 7_000, 12_999, 13_000, 14_900, 15_000, 29_900] {
        assert_eq!(
            qso_timing_row(at(ms), "unknown"),
            qso_timing_row(at(ms), "FT8"),
            "{ms} ms"
        );
        assert_eq!(
            qso_timing_row(at(ms), " ft4 "),
            qso_timing_row(at(ms), "FT4"),
            "{ms} ms"
        );
    }
    assert_eq!(slot_period_ns("unknown"), SLOT_NS);
    assert_eq!(decode_phase_ns("unknown"), decode_phase_ns("FT8"));
}
