//! PAN-386: a caller-chosen guard lets FT8 Fox streams sit 60 Hz apart.

use super::*;
use crate::protocol::ProtocolParams;

fn two_items<'a>(
    params: &'a ProtocolParams,
    symbols: &'a [u8],
    second_offset: f64,
) -> Vec<MultiTxItem<'a>> {
    vec![
        MultiTxItem {
            symbols,
            frequency_offset: 100.0,
            params,
        },
        MultiTxItem {
            symbols,
            frequency_offset: second_offset,
            params,
        },
    ]
}

#[test]
fn guard_9_allows_60_hz_ft8_slots() {
    let params = ProtocolParams::ft8();
    let symbols = [0u8; 79];
    let items = two_items(&params, &symbols, 160.0);
    let samples = modulate_multi_tx_with_guard(&items, 12000, 200.0, 0.5, 9.0)
        .expect("a 9 Hz guard admits FT8 frames 60 Hz apart");
    assert!(!samples.is_empty());
}

#[test]
fn default_guard_still_refuses_60_hz() {
    let params = ProtocolParams::ft8();
    let symbols = [0u8; 79];
    let items = two_items(&params, &symbols, 160.0);
    let err = modulate_multi_tx(&items, 12000, 200.0, 0.5)
        .expect_err("the default 25 Hz guard still refuses 60 Hz");
    assert!(
        err.to_string()
            .contains("Frequency separation 60.0 Hz too small (minimum 75.0 Hz)"),
        "unexpected error text: {err}"
    );
}

#[test]
fn guard_is_still_a_floor() {
    let params = ProtocolParams::ft8();
    let symbols = [0u8; 79];
    let items = two_items(&params, &symbols, 150.0);
    assert!(modulate_multi_tx_with_guard(&items, 12000, 200.0, 0.5, 9.0).is_err());
}
