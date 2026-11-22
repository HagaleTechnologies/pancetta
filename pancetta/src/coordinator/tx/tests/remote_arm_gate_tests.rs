#[test]
fn pan105_all_authority_horizons_deny_before_tick() {
    use pancetta_agent::arm::DenyReason;
    for (source, reason) in [
        (0, DenyReason::Expired),
        (1, DenyReason::CapabilityExpired),
        (2, DenyReason::TxEnablementExpired),
    ] {
        let mut grant = grant();
        match source {
            0 => grant.armed_until_ms = NOW + 20_000,
            1 => grant.capability_exp_ms = NOW + 20_000,
            _ => grant.tx_enabled_until_ms = NOW + 20_000,
        }
        grant.effective_ttl_ms = 20_000;
        let mut state = ArmState::new();
        state.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
        state.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
        state.arm(grant, at(NOW));
        let arm = Arc::new(Mutex::new(state));

        for ch in [ConsentChannel::StationAgent, ConsentChannel::WsjtxUdp] {
            assert!(remote_tx_permitted(&arm, || at(NOW + 19_999), ch));
            assert!(!remote_tx_permitted(&arm, || at(NOW + 20_000), ch));
            assert_eq!(
                arm.lock().unwrap().tx_permit_reason(at(NOW + 20_000), ch),
                pancetta_agent::arm::TxPermit::Denied(reason.clone())
            );
        }

        assert!(
            arm.lock().unwrap().is_armed(),
            "pure checks do not consume the arm"
        );
    }
}

use super::remote_tx_permitted;
use pancetta_agent::arm::{ArmState, ConsentChannel, VerifiedArmGrant};
use std::sync::{Arc, Mutex};

const NOW: i64 = 1_000_000;

/// Both clocks at `ms`: wall = `ms` unix ms, monotonic = a shared origin + `ms`.
fn at(ms: i64) -> pancetta_agent::arm::ArmTime {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(std::time::Instant::now);
    pancetta_agent::arm::ArmTime {
        unix_ms: ms,
        monotonic: base + std::time::Duration::from_millis(ms.unsigned_abs()),
        suspend_aware_ms: ms,
    }
}

fn grant() -> VerifiedArmGrant {
    VerifiedArmGrant {
        operator_callsign: "K5ARH".to_string(),
        ttl_ms: 120_000,
        armed_until_ms: NOW + 120_000,
        capability_exp_ms: NOW + 120_000 + 3_600_000,
        tx_enabled_until_ms: NOW + 120_000 + 3_600_000,
        effective_ttl_ms: 120_000,
        heartbeat_interval_sec: 10,
        scope_tx: true,
        jti: "tx-test-arm-jti".to_string(),
        client_key_id: "tx-test-client-key-id".to_string(),
    }
}

#[test]
fn fresh_unarmed_state_denies() {
    let arm = Arc::new(Mutex::new(ArmState::new()));
    assert!(
        !remote_tx_permitted(&arm, || at(NOW), ConsentChannel::StationAgent),
        "a fresh (unarmed) ArmState must deny remote TX"
    );
}

#[test]
fn armed_consented_fresh_heartbeat_permits() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant(), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    assert!(
        remote_tx_permitted(&arm, || at(NOW), ConsentChannel::StationAgent),
        "armed + tx-scope + consent + fresh heartbeat must permit"
    );
}

#[test]
fn armed_without_local_consent_denies() {
    // Consent is the LOCAL operator gate, per channel: default OFF. Arm
    // with only the station-agent channel's consent on; the WSJT-X UDP
    // channel (no consent of its own) must still be denied.
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant(), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    assert!(
        !remote_tx_permitted(&arm, || at(NOW), ConsentChannel::WsjtxUdp),
        "armed but no local consent on the queried channel must deny"
    );
    assert!(
        remote_tx_permitted(&arm, || at(NOW), ConsentChannel::StationAgent),
        "the channel whose own consent is on stays permitted"
    );
}

#[test]
fn poisoned_lock_fails_closed() {
    // Arm + consent so that IF the lock were readable, it would permit.
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant(), at(NOW));
    let arm = Arc::new(Mutex::new(st));

    // Poison the mutex by panicking while holding the guard.
    let a2 = arm.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = a2.lock().unwrap();
        panic!("poison the arm mutex");
    }));
    assert!(arm.is_poisoned(), "mutex should be poisoned");

    // Even though the underlying state WOULD permit, the poisoned lock must
    // fail CLOSED (deny) — the opposite of tx_qso_is_live's fail-open.
    assert!(
        !remote_tx_permitted(&arm, || at(NOW), ConsentChannel::StationAgent),
        "SAFETY: a poisoned arm lock must fail CLOSED (deny remote TX)"
    );
}

/// Wall clock rolled back `back_ms`, monotonic clock at `mono_ms`.
fn wall_rolled_back(mono_ms: i64, back_ms: i64) -> pancetta_agent::arm::ArmTime {
    pancetta_agent::arm::ArmTime {
        unix_ms: NOW - back_ms,
        monotonic: at(mono_ms).monotonic,
        suspend_aware_ms: at(mono_ms).suspend_aware_ms,
    }
}

/// PAN-97: a wall-clock step backwards cannot keep a silent arm alive —
/// heartbeat silence is measured on the monotonic clock.
#[test]
fn wall_rollback_cannot_extend_the_heartbeat_window() {
    use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant(), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    assert!(remote_tx_permitted(
        &arm,
        || wall_rolled_back(NOW + HEARTBEAT_TIMEOUT_MS - 1, 60_000),
        ConsentChannel::StationAgent
    ));
    assert!(
        !remote_tx_permitted(
            &arm,
            || wall_rolled_back(NOW + HEARTBEAT_TIMEOUT_MS, 60_000),
            ConsentChannel::StationAgent
        ),
        "monotonic silence expires the arm whatever the wall clock says"
    );
}

/// PAN-97: the gate samples the clock only once it holds the arm lock, so
/// a lock wait cannot leave it judging an older instant.
#[test]
fn clock_is_sampled_with_the_lock_held() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant(), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    let probe = arm.clone();
    assert!(remote_tx_permitted(
        &arm,
        || {
            assert!(
                probe.try_lock().is_err(),
                "clock read before the lock was taken"
            );
            at(NOW)
        },
        ConsentChannel::StationAgent
    ));
}
