//! PAN-477: the gain sweep's named FT8 slot math equals the literals it
//! replaced. The sweep stays FT8-only by record (module doc), so these pin
//! that naming the 15 s grid changed no value.

use super::*;

/// The source-slot formula `gain_sweep_runtime.rs` used before PAN-477.
fn previous_source_slot_secs(now: UtcTimestamp, lead_secs: f64) -> i64 {
    (now.as_millisecond() + (lead_secs * 1000.0) as i64).div_euclid(15000) * 15 + 15
}

fn at_ms(ms: i64) -> UtcTimestamp {
    UtcTimestamp::from_millisecond(ms).expect("valid instant")
}

#[test]
fn next_ft8_source_slot_matches_the_previous_formula() {
    let lead = 1.5;
    // 00:00:00.000, 00:00:13.999 and 00:00:14.000 on 2026-01-01, then a
    // pre-1970 instant where `div_euclid` and `/` differ.
    let day = 1_767_225_600_000_i64;
    for (label, now) in [
        ("00:00:00.000", at_ms(day)),
        ("00:00:13.999", at_ms(day + 13_999)),
        ("00:00:14.000", at_ms(day + 14_000)),
        ("pre-1970", at_ms(-86_400_000 * 365 - 7_250)),
    ] {
        assert_eq!(
            next_ft8_source_slot_secs(now, lead),
            previous_source_slot_secs(now, lead),
            "{label}"
        );
    }
    // The boundary lands where the old formula put it: at 13.999 the 15 s
    // boundary (lead 1.5 s, capture boundary 13.5 s) has passed, so the next
    // source slot is 30 s; at 00:00:00 it is 15 s.
    assert_eq!(next_ft8_source_slot_secs(at_ms(day), lead), day / 1000 + 15);
    assert_eq!(
        next_ft8_source_slot_secs(at_ms(day + 13_999), lead),
        day / 1000 + 30
    );
    assert_eq!(FT8_SLOT_SECS, 15);
}

#[test]
fn ft8_parity_index_alternates_every_15_s() {
    let day = 1_767_225_600_i64;
    for k in -4..8_i64 {
        let start = day + k * 15;
        let expected = k.rem_euclid(2) as usize;
        for offset in [0, 7, 14] {
            assert_eq!(
                ft8_slot_parity_index(start + offset),
                expected,
                "slot {k} offset {offset}"
            );
        }
    }
    // Pre-1970 slot starts keep the alternation (div_euclid, not `/`).
    assert_eq!(ft8_slot_parity_index(-15), 1);
    assert_eq!(ft8_slot_parity_index(-30), 0);
    assert_eq!(ft8_slot_parity_index(-1), 1);
}
