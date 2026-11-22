// PAN-443. Dial text and MHz-to-Hz conversion for the WSJT-X decode
// refusal: exact to the Hz, trailing zeros trimmed down to three decimals,
// and an unknown dial for any value that is not a finite positive frequency.
use super::*;

#[test]
fn dial_label_is_exact_and_trims_to_three_decimals() {
    assert_eq!(dial_label(14_074_000), "14.074 MHz");
    assert_eq!(dial_label(14_090_000), "14.090 MHz");
    assert_eq!(dial_label(14_074_500), "14.0745 MHz");
    assert_eq!(dial_label(14_074_011), "14.074011 MHz");
    assert_eq!(dial_label(474_200), "0.4742 MHz");
    assert_eq!(dial_label(136_000), "0.136 MHz");
    assert_eq!(dial_label(1_840_000), "1.840 MHz");
    assert_eq!(dial_label(70_154_000), "70.154 MHz");
}

#[test]
fn hz_from_mhz_rejects_unknown_dials() {
    assert_eq!(hz_from_mhz(14.074), Some(14_074_000));
    assert_eq!(hz_from_mhz(0.4742), Some(474_200));
    assert_eq!(hz_from_mhz(0.0), None);
    assert_eq!(hz_from_mhz(-1.0), None);
    assert_eq!(hz_from_mhz(f64::NAN), None);
    assert_eq!(hz_from_mhz(f64::INFINITY), None);
}
