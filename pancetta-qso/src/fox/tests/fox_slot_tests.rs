//! PAN-386: Fox slot geometry.

use super::*;

#[test]
fn slots_are_300_to_900_in_60_hz_steps() {
    let slots: Vec<f64> = fox_slots().collect();
    assert_eq!(
        slots,
        vec![300.0, 360.0, 420.0, 480.0, 540.0, 600.0, 660.0, 720.0, 780.0, 840.0, 900.0]
    );
}

#[test]
fn lowest_free_slot_skips_fox_streams_60_hz_apart() {
    assert_eq!(fox_slot_offset(&[], &[]), Some(300.0));
    assert_eq!(fox_slot_offset(&[300.0], &[]), Some(360.0));
    assert_eq!(fox_slot_offset(&[300.0, 420.0], &[]), Some(360.0));
}

#[test]
fn other_streams_keep_75_hz_clearance() {
    // 300 and 360 are both within 75 Hz of 330.
    assert_eq!(fox_slot_offset(&[], &[330.0]), Some(420.0));
}

#[test]
fn no_free_slot_returns_none() {
    let all: Vec<f64> = fox_slots().collect();
    assert_eq!(fox_slot_offset(&all, &[]), None);
}

#[test]
fn fox_slots_apply_only_to_ft8() {
    assert!(fox_slots_apply("FT8"));
    assert!(fox_slots_apply("ft8"));
    assert!(!fox_slots_apply("FT4"));
    assert!(!fox_slots_apply("FT2"));
}
