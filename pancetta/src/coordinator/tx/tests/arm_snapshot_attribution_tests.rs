#[test]
fn pan105_all_authority_horizons_deny_before_tick() {
    use pancetta_agent::arm::DenyReason;
    for (source, reason) in [
        (0, DenyReason::Expired),
        (1, DenyReason::CapabilityExpired),
        (2, DenyReason::TxEnablementExpired),
    ] {
        let mut grant = grant_for("N0CALL", "client-a");
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

        for ms in [19_999, 20_000] {
            let snapshot = ArmSnapshot::take(&arm, || at(NOW + ms));
            for identity in [Some("client-a"), Some("client-b"), None] {
                assert_eq!(
                    snapshot.permits(identity),
                    super::remote_tx_permitted_for(&arm, || at(NOW + ms), identity)
                );
            }
            assert_eq!(snapshot.permits(Some("client-a")), ms < 20_000);
            assert_eq!(snapshot.operator_callsign.as_deref(), Some("N0CALL"));
        }
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

use super::ArmSnapshot;
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

fn grant_for(operator_callsign: &str, client_key_id: &str) -> VerifiedArmGrant {
    VerifiedArmGrant {
        operator_callsign: operator_callsign.to_string(),
        ttl_ms: 120_000,
        armed_until_ms: NOW + 120_000,
        capability_exp_ms: NOW + 120_000 + 3_600_000,
        tx_enabled_until_ms: NOW + 120_000 + 3_600_000,
        effective_ttl_ms: 120_000,
        heartbeat_interval_sec: 10,
        scope_tx: true,
        jti: "tx-snapshot-test-jti".to_string(),
        client_key_id: client_key_id.to_string(),
    }
}

/// Round-10 review (Codex P2): a detached denial-reporting task must
/// attribute using the arm state AS IT WAS when the snapshot was taken,
/// not whoever happens to be armed by the time the task actually runs.
#[test]
fn attribution_reflects_the_snapshot_instant_not_a_later_live_lock() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant_for("K5ARH", "client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));

    let snapshot = ArmSnapshot::take(&arm, || at(NOW));

    // A DIFFERENT operator arms the live state AFTER the snapshot was
    // taken — simulating the race a detached spawn's later re-lock
    // would be vulnerable to.
    arm.lock()
        .unwrap()
        .arm(grant_for("W1AW", "client-b"), at(NOW + 10));

    assert_eq!(
        snapshot.operator_attribution(Some("client-a")).as_deref(),
        Some("K5ARH"),
        "attribution must reflect who was armed AT THE SNAPSHOT INSTANT, \
         not whoever armed later"
    );
}

#[test]
fn identity_mismatch_never_attributes_to_the_armed_operator() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant_for("K5ARH", "client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    let snapshot = ArmSnapshot::take(&arm, || at(NOW));

    assert_eq!(
        snapshot.operator_attribution(Some("client-b")),
        None,
        "a frame bound to a DIFFERENT client than the one armed must \
         never be attributed to the armed operator"
    );
}

#[test]
fn unbound_frame_attributes_to_whoever_is_armed() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    st.arm(grant_for("K5ARH", "client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));
    let snapshot = ArmSnapshot::take(&arm, || at(NOW));

    assert_eq!(
        snapshot.operator_attribution(None).as_deref(),
        Some("K5ARH"),
        "a frame with no bound identity falls back to whoever is armed, \
         preserving pre-PAN-91 attribution"
    );
}

/// Round-13 review (Codex P1): `is_identity_mismatch` is gated on
/// `tx_permitted` (it exists to pick DENIAL-REASON WORDING), so reusing
/// it for attribution let a mismatched-AND-otherwise-denied frame (here:
/// local consent off, so `tx_permitted` is false) fall through to
/// attributing B's callsign on A's denied frame — the gate suppressed
/// the identity check instead of the identity check standing on its
/// own. Attribution must refuse whenever the identity genuinely
/// differs, independent of why `tx_permitted` is false.
#[test]
fn identity_mismatch_refuses_attribution_even_when_tx_is_not_permitted_for_another_reason() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant_for("K5ARH", "client-b"), at(NOW));
    // Local consent OFF (revoked after the arm): tx_permitted is false
    // for an UNRELATED reason, not because of the identity mismatch below.
    st.set_channel_consent(ConsentChannel::StationAgent, false, NOW);
    let arm = Arc::new(Mutex::new(st));
    let snapshot = ArmSnapshot::take(&arm, || at(NOW));
    assert!(
        !snapshot.station_agent_permitted,
        "test setup: tx must not be permitted here"
    );
    assert_eq!(
        snapshot.armed_client_key_id.as_deref(),
        Some("client-b"),
        "test setup: client-b holds the arm"
    );

    assert_eq!(
        snapshot.operator_attribution(Some("client-a")),
        None,
        "a frame bound to client-a must never be attributed to client-b's \
         operator, even when the denial's proximate cause is something \
         else entirely (local consent off)"
    );
}

/// PAN-98: the snapshot mirrors `remote_tx_permitted_for`'s channel
/// selection — a `None`-identity frame reads the WSJT-X UDP channel's
/// consent, a `Some` frame the station-agent channel's.
#[test]
fn snapshot_permits_none_uses_wsjtx_bit_and_some_uses_station_agent_bit() {
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant_for("K5ARH", "client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));

    // SA consent on, wsjtx consent off.
    let snapshot = ArmSnapshot::take(&arm, || at(NOW));
    assert!(snapshot.permits(Some("client-a")));
    assert!(
        !snapshot.permits(None),
        "a None-origin frame needs the WSJT-X UDP channel's own consent"
    );

    // Revoke SA consent, grant wsjtx consent.
    {
        let mut st = arm.lock().unwrap();
        st.set_channel_consent(ConsentChannel::StationAgent, false, NOW);
        st.set_channel_consent(ConsentChannel::WsjtxUdp, true, NOW);
    }
    let snapshot = ArmSnapshot::take(&arm, || at(NOW));
    assert!(
        !snapshot.permits(Some("client-a")),
        "a Some-origin frame needs the station-agent channel's own consent"
    );
    assert!(
        !snapshot.is_identity_mismatch(Some("client-a")),
        "a consent denial is not an identity problem"
    );
    assert!(snapshot.permits(None));
}

/// Wall clock rolled back `back_ms`, monotonic clock at `mono_ms`.
fn wall_rolled_back(mono_ms: i64, back_ms: i64) -> pancetta_agent::arm::ArmTime {
    pancetta_agent::arm::ArmTime {
        unix_ms: NOW - back_ms,
        monotonic: at(mono_ms).monotonic,
        suspend_aware_ms: at(mono_ms).suspend_aware_ms,
    }
}

/// PAN-97: the snapshot's permission bit uses the monotonic dead-man
/// window, and its audit timestamp is the wall time sampled under the
/// same lock.
#[test]
fn snapshot_denies_after_monotonic_expiry_despite_wall_rollback() {
    use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
    let mut st = ArmState::new();
    st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
    st.arm(grant_for("K5ARH", "client-a"), at(NOW));
    let arm = Arc::new(Mutex::new(st));

    let fresh = ArmSnapshot::take(&arm, || wall_rolled_back(NOW + 1_000, 60_000));
    assert!(fresh.permits(Some("client-a")));
    assert_eq!(fresh.decided_at_unix_ms, NOW - 60_000);

    let lapsed = ArmSnapshot::take(&arm, || {
        wall_rolled_back(NOW + HEARTBEAT_TIMEOUT_MS, 60_000)
    });
    assert!(!lapsed.permits(Some("client-a")));
    assert!(!lapsed.permits(None));
    // Attribution is still frozen under the same lock.
    assert_eq!(
        lapsed.operator_attribution(Some("client-a")).as_deref(),
        Some("K5ARH")
    );
}
