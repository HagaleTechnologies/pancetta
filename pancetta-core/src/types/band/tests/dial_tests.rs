//! PAN-443: the shared dial predicates. `same_dial` and `is_band_change`
//! live here so the coordinator and the TUI judge a dial by one rule.

use super::*;

#[test]
fn same_dial_allows_ten_hz_and_treats_zero_as_unknown() {
    assert_eq!(SAME_DIAL_SLACK_HZ, 10);
    assert!(same_dial(14_074_000, 14_074_000));
    assert!(same_dial(14_074_010, 14_074_000));
    assert!(same_dial(14_073_990, 14_074_000));
    assert!(same_dial(70_154_000, 70_154_000));
    assert!(!same_dial(14_074_011, 14_074_000));
    assert!(!same_dial(7_074_000, 14_074_000));
    assert!(!same_dial(0, 14_074_000), "an unknown dial is never there");
}

#[test]
fn is_band_change_compares_named_bands_by_name() {
    assert!(!is_band_change(14_074_000, 14_090_000));
    assert!(is_band_change(14_074_000, 7_074_000));
    assert!(!is_band_change(14_074_000, 14_074_000));
}

#[test]
fn is_band_change_uses_100_khz_off_the_band_table() {
    assert_eq!(BAND_CHANGE_HZ_THRESHOLD, 100_000);
    assert!(is_band_change(14_074_000, 70_154_000));
    assert!(is_band_change(70_154_000, 14_074_000));
    assert!(!is_band_change(70_154_000, 70_200_000));
    assert!(is_band_change(70_154_000, 70_254_000));
    assert!(is_band_change(474_200, 136_000));
}

#[test]
fn is_band_change_ignores_an_unknown_old_dial() {
    assert!(!is_band_change(0, 14_074_000));
}

#[test]
fn freq_command_settle_window_is_three_seconds() {
    assert_eq!(FREQ_COMMAND_SETTLE_MS, 3_000);
}
