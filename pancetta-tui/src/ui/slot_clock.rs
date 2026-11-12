//! Slot progress and station TX parity for the shared title row.
use chrono::{DateTime, Utc};
use pancetta_core::slot::{current_slot_start_with_period, SlotParity, SLOT_NS};

fn slot_period_ns(mode: &str) -> i64 {
    match mode.trim().to_ascii_uppercase().as_str() {
        "FT4" => 7_500_000_000,
        "FT2" => 3_200_000_000,
        _ => SLOT_NS,
    }
}

/// Format the current slot and the separately resolved station TX parity.
pub fn slot_clock_text(now: DateTime<Utc>, mode: &str, tx: Option<SlotParity>) -> String {
    let period = slot_period_ns(mode);
    let elapsed = (now - current_slot_start_with_period(now, period))
        .num_nanoseconds()
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
mod tests {
    use super::*;

    fn at(ms: i64) -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
            + chrono::Duration::milliseconds(ms)
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
