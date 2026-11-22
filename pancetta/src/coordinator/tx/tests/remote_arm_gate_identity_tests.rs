#[test]
fn pan105_all_authority_horizons_deny_before_tick() {
    use pancetta_agent::arm::DenyReason;
    for (source, reason) in [
        (0, DenyReason::Expired),
        (1, DenyReason::CapabilityExpired),
        (2, DenyReason::TxEnablementExpired),
    ] {
        let mut grant = grant_for("client-a");
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

        for identity in [Some("client-a"), None] {
            assert!(remote_tx_permitted_for(&arm, || at(NOW + 19_999), identity));
            assert!(!remote_tx_permitted_for(
                &arm,
                || at(NOW + 20_000),
                identity
            ));
        }
        assert!(!remote_tx_permitted_for(&arm, || at(NOW), Some("client-b")));
        assert_eq!(
            arm.lock()
                .unwrap()
                .tx_permit_reason(at(NOW + 20_000), ConsentChannel::StationAgent),
            pancetta_agent::arm::TxPermit::Denied(reason)
        );

        assert!(
            arm.lock().unwrap().is_armed(),
            "pure checks do not consume the arm"
        );
    }
}

use super::remote_tx_permitted_for;
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

fn grant_for(client_key_id: &str) -> VerifiedArmGrant {
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
        client_key_id: client_key_id.to_string(),
    }
}

fn armed_and_consented(client_key_id: &str) -> Arc<Mutex<ArmState>> {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant_for(client_key_id), at(NOW));
    Arc::new(Mutex::new(st))
}

#[test]
fn matching_client_is_permitted() {
    let arm = armed_and_consented("client-a");
    assert!(
        remote_tx_permitted_for(&arm, || at(NOW), Some("client-a")),
        "the client that is actually armed must be permitted to TX"
    );
}

#[test]
fn different_client_is_denied_even_though_someone_is_armed() {
    // THE regression this fix closes: client-b's frame must not be
    // authorized just because client-a happens to be armed right now.
    let arm = armed_and_consented("client-a");
    assert!(
        !remote_tx_permitted_for(&arm, || at(NOW), Some("client-b")),
        "PAN-91: a frame bound to a different client than the one \
         currently armed must be denied, not authorized by proxy"
    );
}

#[test]
fn no_bound_identity_falls_back_to_boolean_only_gate() {
    // Pre-PAN-91 producers (no client identity concept, e.g. the WSJT-X
    // UDP bridge) must be unaffected: None degrades to the old
    // boolean-only check.
    let arm = armed_and_consented("client-a");
    assert!(
        remote_tx_permitted_for(&arm, || at(NOW), None),
        "a frame with no bound client identity must fall back to the \
         plain armed/permitted check, preserving pre-PAN-91 behavior"
    );
}

#[test]
fn bound_identity_denied_when_nobody_armed() {
    let arm = Arc::new(Mutex::new(ArmState::new()));
    assert!(
        !remote_tx_permitted_for(&arm, || at(NOW), Some("client-a")),
        "an unarmed station must deny regardless of the frame's bound identity"
    );
}

/// PAN-98: a `None`-identity frame originates on the WSJT-X UDP channel
/// and requires THAT channel's own consent — the station-agent channel's
/// consent (which the arm itself needed) never satisfies it.
#[test]
fn none_origin_requires_wsjtx_channel_consent() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant_for("client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    assert!(
        !remote_tx_permitted_for(&arm, || at(NOW), None),
        "a WSJT-X-UDP-origin frame must be denied while that channel's own \
         consent is off, even though the arm is live"
    );
    arm.lock()
        .unwrap()
        .set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    assert!(
        remote_tx_permitted_for(&arm, || at(NOW), None),
        "with the WSJT-X UDP channel's own consent on, its frame is permitted"
    );
}

/// PAN-98: transmission-time re-checks the ORIGINATING channel's own
/// CURRENT consent — revoking the station-agent channel's consent after
/// a valid arm denies that channel's frames even though the WSJT-X UDP
/// channel's consent is still on.
#[test]
fn some_origin_denied_when_station_agent_consent_revoked_after_arm() {
    let arm = armed_and_consented("client-a");
    assert!(remote_tx_permitted_for(&arm, || at(NOW), Some("client-a")));
    arm.lock()
        .unwrap()
        .set_channel_consent(ConsentChannel::StationAgent, false, NOW);
    assert!(
        !remote_tx_permitted_for(&arm, || at(NOW), Some("client-a")),
        "transmission-time must re-check the ORIGINATING channel's own current consent"
    );
    assert!(
        remote_tx_permitted_for(&arm, || at(NOW), None),
        "the WSJT-X UDP channel's own consent still governs its frames"
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

/// PAN-97: the identity-bound gate also measures liveness monotonically.
#[test]
fn wall_rollback_cannot_extend_the_heartbeat_window_for_a_bound_client() {
    use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
    let arm = armed_and_consented("client-a");
    assert!(remote_tx_permitted_for(
        &arm,
        || wall_rolled_back(NOW + HEARTBEAT_TIMEOUT_MS - 1, 60_000),
        Some("client-a")
    ));
    assert!(!remote_tx_permitted_for(
        &arm,
        || wall_rolled_back(NOW + HEARTBEAT_TIMEOUT_MS, 60_000),
        Some("client-a")
    ));
}
