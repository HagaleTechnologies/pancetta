//! PAN-477: the TX worker's protocol-scaled values agree with the shared
//! `pancetta_qso::tx_slot` scaling the QSO manager's keep-call ledger reads,
//! for every protocol the build has and every `tx_late_max_ms` in
//! 0..=20000 ms.

use super::*;

fn protocols() -> Vec<pancetta_ft8::Protocol> {
    vec![
        pancetta_ft8::Protocol::Ft8,
        pancetta_ft8::Protocol::Ft4,
        #[cfg(feature = "ft2")]
        pancetta_ft8::Protocol::Ft2,
    ]
}

#[test]
fn worker_scaling_equals_the_shared_tx_slot_scaling() {
    use pancetta_qso::tx_slot::{current_slot_acceptance_cap_ms as shared_cap, scale_ms_to_slot};

    // The worker's constants are the shared ones.
    assert_eq!(
        COALESCE_COLLECT_WINDOW_MS,
        pancetta_qso::tx_slot::COALESCE_COLLECT_WINDOW_MS
    );
    assert_eq!(
        COALESCE_CAP_SAFETY_MARGIN_MS,
        pancetta_qso::tx_slot::COALESCE_CAP_SAFETY_MARGIN_MS
    );

    for protocol in protocols() {
        let slot_ns = pancetta_ft8::ProtocolParams::from_protocol(protocol).slot_ns();
        assert_eq!(
            coalesce_collect_window_ms(protocol),
            scale_ms_to_slot(800, slot_ns),
            "{protocol:?} base coalesce window"
        );
        assert_eq!(
            coalesce_max_extension_ms(protocol),
            scale_ms_to_slot(COALESCE_MAX_EXTENSION_MS, slot_ns),
            "{protocol:?} coalesce extension ceiling"
        );
        for ms in 0..=20_000_u64 {
            assert_eq!(
                tx_late_max_ms_effective(protocol, ms),
                scale_ms_to_slot(ms, slot_ns),
                "{protocol:?} tx_late_max_ms_effective at {ms} ms"
            );
            let cap = current_slot_acceptance_cap_ms(protocol, ms);
            assert_eq!(
                cap,
                shared_cap(slot_ns, ms),
                "{protocol:?} acceptance cap at {ms} ms"
            );
            // The delegated cap is still the PAN-117 composition of the
            // worker's own scaled terms.
            assert_eq!(
                cap,
                tx_late_max_ms_effective(protocol, ms)
                    .saturating_sub(coalesce_collect_window_ms(protocol))
                    .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS),
                "{protocol:?} acceptance cap composition at {ms} ms"
            );
        }
    }
}
