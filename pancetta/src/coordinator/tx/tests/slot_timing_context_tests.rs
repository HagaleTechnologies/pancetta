use super::SlotTimingContext;

fn ctx(
    active_protocol: pancetta_ft8::Protocol,
    tx_late_max_ms: u64,
    ptt_lead_ms: u64,
) -> SlotTimingContext {
    SlotTimingContext {
        slot_clock: pancetta_core::SlotClock::without_step_detection(
            pancetta_core::time::system_utc_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                pancetta_core::slot::SLOT_NS,
            )),
        ),
        active_protocol,
        tx_late_max_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(tx_late_max_ms)),
        tx_self_parity: pancetta_config::station::TxSelfParity::Auto,
        ptt_lead_ms,
        sample_rate: 12_000,
        slot_ns: pancetta_core::slot::SLOT_NS,
        request_received_at: pancetta_core::time::UtcTimestamp::now(),
        request_resync_epoch: 0,
    }
}

#[test]
fn late_cap_matches_tx_late_max_ms_effective_for_every_protocol() {
    assert_eq!(
        ctx(pancetta_ft8::Protocol::Ft8, 3000, 80).late_cap_ms(),
        3000
    );
    assert_eq!(
        ctx(pancetta_ft8::Protocol::Ft4, 3000, 80).late_cap_ms(),
        1500
    );
    #[cfg(feature = "ft2")]
    assert_eq!(
        ctx(pancetta_ft8::Protocol::Ft2, 3000, 80).late_cap_ms(),
        super::tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft2, 3000)
    );
}

#[test]
fn ptt_target_is_target_slot_minus_ptt_lead() {
    let target_slot = pancetta_core::time::UtcTimestamp::now();
    for lead_ms in [80u64, 2000] {
        assert_eq!(
            ctx(pancetta_ft8::Protocol::Ft8, 3000, lead_ms).ptt_target(target_slot),
            target_slot - jiff::SignedDuration::from_millis(lead_ms as i64)
        );
    }
}
