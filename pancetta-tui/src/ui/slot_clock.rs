//! Slot progress and station TX parity for the shared title row, and the
//! QSO panel's timing row.
//!
//! PAN-477: both read the wall clock once per render (`UtcTimestamp::now()`,
//! the same wall value the coordinator's shared `SlotClock::now()` returns;
//! the TUI holds no `SlotClock` and a display has no deadline to re-anchor)
//! and the active mode's slot grid from the tables here. A `pancetta` test
//! (`tui_slot_tables_match_dsp_timing`) pins both tables to the DSP's
//! `derive_dsp_timing`.
use jiff::SignedDuration;
use pancetta_core::slot::{current_slot_start_with_period, SlotParity, SLOT_NS};
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

const NS_PER_SEC: i64 = 1_000_000_000;

/// Slot length of `mode` in nanoseconds: FT8 15 s, FT4 7.5 s, FT2 3.2 s. A
/// mode string the TUI does not know uses FT8's grid.
pub fn slot_period_ns(mode: &str) -> i64 {
    match mode.trim().to_ascii_uppercase().as_str() {
        "FT4" => 7_500_000_000,
        "FT2" => 3_200_000_000,
        _ => SLOT_NS,
    }
}

/// Time into a slot at which `mode`'s decode fires, in nanoseconds: FT8 13 s,
/// FT4 6.5 s, FT2 2.2 s (the slot minus the DSP's decode margin). A mode
/// string the TUI does not know uses FT8's.
pub fn decode_phase_ns(mode: &str) -> i64 {
    match mode.trim().to_ascii_uppercase().as_str() {
        "FT4" => 6_500_000_000,
        "FT2" => 2_200_000_000,
        _ => 13 * NS_PER_SEC,
    }
}

/// The QSO panel's timing row for one instant (PAN-477).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QsoTimingRow {
    /// `TX Window` once the slot reaches the mode's decode point, else
    /// `RX Period`.
    pub label: &'static str,
    /// Gauge fill: whole seconds into the slot over the slot length.
    pub ratio: f64,
    /// Countdown label: whole seconds left in the slot, rounded up.
    pub remaining_secs: i64,
    /// Start of the slot `now` falls in, on the mode's grid.
    pub slot_start: UtcTimestamp,
    /// Parity of that slot on the mode's grid.
    pub parity: SlotParity,
}

/// Build the QSO panel's timing row for `now` on `mode`'s slot grid. For FT8
/// the label, fill and countdown equal the historical whole-second values
/// (`RX Period` before 13 s, fill `s/15`, countdown `15 - s`).
pub fn qso_timing_row(now: UtcTimestamp, mode: &str) -> QsoTimingRow {
    let period = slot_period_ns(mode);
    let slot_start = current_slot_start_with_period(now, period);
    let elapsed = (now - slot_start)
        .as_nanos_i64()
        .unwrap_or(0)
        .clamp(0, period);
    let label = if elapsed >= decode_phase_ns(mode) {
        "TX Window"
    } else {
        "RX Period"
    };
    let whole_secs = elapsed / NS_PER_SEC;
    let period_secs = period as f64 / NS_PER_SEC as f64;
    let ratio = (whole_secs as f64 / period_secs).clamp(0.0, 1.0);
    let remaining_secs = (period - elapsed + NS_PER_SEC - 1) / NS_PER_SEC;
    QsoTimingRow {
        label,
        ratio,
        remaining_secs,
        slot_start,
        parity: SlotParity::of_with_period(now, period),
    }
}

/// Format the current slot and the separately resolved station TX parity.
pub fn slot_clock_text(now: UtcTimestamp, mode: &str, tx: Option<SlotParity>) -> String {
    let period = slot_period_ns(mode);
    let elapsed = (now - current_slot_start_with_period(now, period))
        .as_nanos_i64()
        .unwrap_or(0)
        .clamp(0, period);
    let letter = |parity| match parity {
        SlotParity::Even => 'E',
        SlotParity::Odd => 'O',
    };
    let current = letter(SlotParity::of_with_period(now, period));
    let tx = tx.map(letter).unwrap_or('-');
    let filled = ((elapsed * 6 + period / 2) / period).min(6) as usize;
    let bar = format!("{}{}", "▮".repeat(filled), "▯".repeat(6 - filled));
    let secs = elapsed / 1_000_000_000;
    format!("[{current} {bar} {secs:02}s │ TX:{tx}]")
}

#[cfg(test)]
#[path = "slot_clock/tests/qso_timing_row_tests.rs"]
mod qso_timing_row_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> UtcTimestamp {
        "2026-01-01T00:00:00Z".parse::<UtcTimestamp>().unwrap() + SignedDuration::from_millis(ms)
    }

    #[test]
    fn slot_clock_text_formats_ft8_boundary_mid_and_end() {
        for (ms, tx, expected) in [
            (0, None, "[E ▯▯▯▯▯▯ 00s │ TX:-]"),
            (6000, Some(SlotParity::Odd), "[E ▮▮▯▯▯▯ 06s │ TX:O]"),
            (14900, Some(SlotParity::Even), "[E ▮▮▮▮▮▮ 14s │ TX:E]"),
            (15000, None, "[O ▯▯▯▯▯▯ 00s │ TX:-]"),
        ] {
            assert_eq!(slot_clock_text(at(ms), "FT8", tx), expected);
        }
    }

    #[test]
    fn slot_clock_text_uses_the_mode_period() {
        assert_eq!(
            slot_clock_text(at(3750), "FT4", None),
            "[E ▮▮▮▯▯▯ 03s │ TX:-]"
        );
        assert_eq!(
            slot_clock_text(at(3300), "FT2", None),
            "[O ▯▯▯▯▯▯ 00s │ TX:-]"
        );
        assert_eq!(
            slot_clock_text(at(7600), "FT4", None),
            "[O ▯▯▯▯▯▯ 00s │ TX:-]"
        );
        assert_eq!(
            slot_clock_text(at(3750), " ft4 ", None),
            slot_clock_text(at(3750), "FT4", None)
        );
        assert_eq!(
            slot_clock_text(at(7600), "unknown", None),
            slot_clock_text(at(7600), "FT8", None)
        );
    }

    #[test]
    fn slot_clock_text_is_always_21_columns() {
        for mode in ["FT8", "FT4", "FT2", "unknown"] {
            for ms in (0..slot_period_ns(mode) / 1_000_000 * 2).step_by(100) {
                for tx in [None, Some(SlotParity::Even), Some(SlotParity::Odd)] {
                    assert_eq!(
                        ratatui::text::Span::raw(slot_clock_text(at(ms), mode, tx)).width(),
                        21
                    );
                }
            }
        }
    }
}
