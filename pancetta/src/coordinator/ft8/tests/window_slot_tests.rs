//! PAN-477: the decoder stamps parity, the desense slot and the cross-time
//! period from the slot the DSP cut the window from (`WindowSlot`), never
//! from its own receipt time.

use crate::coordinator::pipeline::WindowSlot;
use jiff::SignedDuration;
use pancetta_core::slot::SlotParity;
use pancetta_core::time::UtcTimestamp;

const FT8_NS: i64 = 15_000_000_000;
const FT4_NS: i64 = 7_500_000_000;
const FT2_NS: i64 = 3_200_000_000;

/// 2026-01-01 00:00:00 UTC, a boundary for FT8, FT4 and FT2.
fn base() -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn slot(index: i64, slot_ns: i64) -> WindowSlot {
    WindowSlot {
        start: base() + SignedDuration::from_nanos(index * slot_ns),
        slot_ns,
    }
}

/// The window's parity is its cut slot's parity on its own period, for
/// every mode: consecutive slots alternate, starting Even at 00:00:00.
#[test]
fn window_slot_parity_is_the_cut_slot_parity_in_every_mode() {
    for slot_ns in [FT8_NS, FT4_NS, FT2_NS] {
        for index in 0..6 {
            let expected = if index % 2 == 0 {
                SlotParity::Even
            } else {
                SlotParity::Odd
            };
            assert_eq!(
                slot(index, slot_ns).parity(),
                expected,
                "slot {index} of a {slot_ns} ns period"
            );
        }
    }

    // FT8 regression (the receipt-time helper's old FT8 case): a window
    // cut from the slot at Unix 0 is Even, as `SlotParity::of` says.
    let ft8_epoch = WindowSlot {
        start: UtcTimestamp::from_nanosecond_i64(0),
        slot_ns: FT8_NS,
    };
    assert_eq!(ft8_epoch.parity(), SlotParity::Even);
    assert_eq!(ft8_epoch.parity(), SlotParity::of(ft8_epoch.start));

    // The same instant on FT4's period is the period's own parity: the
    // slot starting at 7.5 s is FT4 Odd but lies in FT8's Even slot.
    let ft4_second = WindowSlot {
        start: UtcTimestamp::from_nanosecond_i64(7_500_000_000),
        slot_ns: FT4_NS,
    };
    assert_eq!(ft4_second.parity(), SlotParity::Odd);
    assert_eq!(SlotParity::of(ft4_second.start), SlotParity::Even);
}

/// The desense observation's slot start and length come from the window:
/// the start in Unix ms (clamped at 0 before 1970), the length in ms (at
/// least 1).
#[test]
fn window_slot_start_and_len_feed_the_desense_observation() {
    for slot_ns in [FT8_NS, FT4_NS, FT2_NS] {
        let window = slot(3, slot_ns);
        assert_eq!(
            window.start_ms(),
            (base().as_millisecond() + 3 * slot_ns / 1_000_000) as u64
        );
        assert_eq!(window.len_ms(), (slot_ns / 1_000_000) as u64);
    }

    let pre_epoch = WindowSlot {
        start: UtcTimestamp::from_nanosecond_i64(-FT8_NS),
        slot_ns: FT8_NS,
    };
    assert_eq!(pre_epoch.start_ms(), 0, "clamped at 0");

    let degenerate = WindowSlot {
        start: base(),
        slot_ns: 0,
    };
    assert_eq!(degenerate.len_ms(), 1, "at least 1 ms");
}
