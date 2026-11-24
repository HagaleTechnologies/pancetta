//! PAN-477: the one TX current-slot rule `schedule_tx` and
//! `keep_call_tx_slot` share.

use super::*;
use jiff::SignedDuration;
use pancetta_core::slot::SlotParity;

const FT8_NS: i64 = 15_000_000_000;
const FT4_NS: i64 = 7_500_000_000;
const FT2_NS: i64 = 3_200_000_000;

/// 2026-01-01T00:00:00Z is 1767225600 s = 15 * 117815040 = 3.2 * 552258000:
/// an Even slot start on the FT8, FT4 and FT2 grids.
fn base() -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn ms(n: i64) -> SignedDuration {
    SignedDuration::from_millis(n)
}

#[test]
fn current_slot_is_kept_only_within_the_cap_and_on_parity() {
    for (slot_ns, cap) in [(FT8_NS, 1700_u64), (FT4_NS, 600), (FT2_NS, 0)] {
        let slot_ms = slot_ns / 1_000_000;
        let b = base();
        assert_eq!(
            SlotParity::of_with_period(b, slot_ns),
            SlotParity::Even,
            "base is an Even slot start for slot_ns {slot_ns}"
        );

        // On parity, from the boundary up to and including the cap: current.
        for at in [0, cap as i64 / 2, cap as i64] {
            assert_eq!(
                tx_target_slot(b + ms(at), SlotParity::Even, cap, slot_ns),
                (b, false),
                "{at} ms into an own slot is within the {cap} ms cap (slot_ns {slot_ns})"
            );
        }

        // On parity, 1 ms past the cap: the next own slot, two periods on.
        assert_eq!(
            tx_target_slot(b + ms(cap as i64 + 1), SlotParity::Even, cap, slot_ns),
            (b + ms(2 * slot_ms), true),
            "1 ms past the cap defers (slot_ns {slot_ns})"
        );

        // Off parity, even at the boundary: the next slot, which has the
        // wanted parity.
        assert_eq!(
            tx_target_slot(b, SlotParity::Odd, cap, slot_ns),
            (b + ms(slot_ms), true),
            "the wrong parity always defers (slot_ns {slot_ns})"
        );
        assert_eq!(
            tx_target_slot(b + ms(slot_ms - 1), SlotParity::Odd, u64::MAX, slot_ns),
            (b + ms(slot_ms), true),
            "an unbounded cap never keeps an off-parity slot (slot_ns {slot_ns})"
        );

        // Inside the next (Odd) slot, wanting Odd within the cap: current.
        assert_eq!(
            tx_target_slot(b + ms(slot_ms), SlotParity::Odd, cap, slot_ns),
            (b + ms(slot_ms), false)
        );
    }
}

#[test]
fn acceptance_cap_matches_the_worker_values_per_mode() {
    // 3000 - 800 - 500 = 1700 (FT8); 1500 - 400 - 500 = 600 (FT4);
    // 640 - 171 - 500 saturates to 0 (FT2).
    assert_eq!(current_slot_acceptance_cap_ms(FT8_NS, 3000), 1700);
    assert_eq!(current_slot_acceptance_cap_ms(FT4_NS, 3000), 600);
    assert_eq!(current_slot_acceptance_cap_ms(FT2_NS, 3000), 0);
    assert_eq!(current_slot_acceptance_cap_ms(FT8_NS, 1000), 0);

    // The scaling is the identity on FT8 and the slot ratio elsewhere.
    for ms in [0_u64, 1, 799, 800, 3000, 20_000] {
        assert_eq!(scale_ms_to_slot(ms, FT8_NS), ms);
    }
    assert_eq!(scale_ms_to_slot(800, FT4_NS), 400);
    assert_eq!(scale_ms_to_slot(800, FT2_NS), 171);
    assert_eq!(scale_ms_to_slot(3000, FT2_NS), 640);
}
