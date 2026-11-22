//! Round-2 review (Codex P1): the identity-bound arm gate must be
//! rechecked immediately before PTT, not only at Step 4b-arm (before
//! the audio-buffer build / late-pivot re-encode). The new Step 4d-arm
//! recheck (single-TX and multi-TX) reuses [`emit_disarm_interrupt_signals`]
//! — generalized here to take a caller-supplied `diagnostic_verb` and
//! `denial_reason` so a pre-PTT denial doesn't misreport "disarmed
//! mid-transmission" for a frame that was never keyed. These tests pin
//! that generalization: the "denied" wording flows through to the TUI
//! diagnostic, the audit record, and the client-visible relay exactly
//! as given, and operator attribution still degrades to `None` on an
//! identity mismatch (not to whoever else happens to be armed).
use super::*;
use crate::message_bus::MessageBus;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// PAN-147: guarded path (pre-cleaned, deleted on drop) — see
/// [`crate::coordinator::audit_test_util`].
fn audit_tmp() -> crate::coordinator::audit_test_util::TempPath {
    crate::coordinator::audit_test_util::TempPath::new("pancetta-tx-disarm-signal-test")
}

fn read_audit_lines(path: &std::path::Path) -> Vec<pancetta_agent::audit::AuditEvent> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("audit line must be valid JSON"))
        .collect()
}

mod tx_send_authorized_tests {
    use super::*;
    use crate::message_bus::TxOrigin;
    use pancetta_agent::arm::{ArmState, ArmTime, ConsentChannel, VerifiedArmGrant};

    fn armed_client_a() -> Arc<std::sync::Mutex<ArmState>> {
        let mut state = ArmState::new();
        let now_ms = UtcTimestamp::now().as_millisecond();
        state.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
        state.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".to_string(),
                ttl_ms: 120_000,
                armed_until_ms: now_ms + 120_000,
                capability_exp_ms: now_ms + 120_000 + 3_600_000,
                tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
                effective_ttl_ms: 120_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan134-predicate".to_string(),
                client_key_id: "client-a".to_string(),
            },
            ArmTime::now(),
        );
        Arc::new(std::sync::Mutex::new(state))
    }

    #[test]
    fn local_origin_is_always_authorized() {
        let arm = Arc::new(std::sync::Mutex::new(ArmState::new()));
        assert!(tx_send_authorized(TxOrigin::Local, &arm, None));
    }

    #[test]
    fn remote_origin_authorized_when_armed_as_same_client() {
        assert!(tx_send_authorized(
            TxOrigin::Remote,
            &armed_client_a(),
            Some("client-a")
        ));
    }

    #[test]
    fn remote_origin_denied_on_identity_mismatch() {
        assert!(!tx_send_authorized(
            TxOrigin::Remote,
            &armed_client_a(),
            Some("client-b")
        ));
    }

    #[test]
    fn remote_origin_denied_when_disarmed() {
        let arm = armed_client_a();
        arm.lock().unwrap().set_channel_consent(
            ConsentChannel::StationAgent,
            false,
            UtcTimestamp::now().as_millisecond(),
        );
        assert!(!tx_send_authorized(
            TxOrigin::Remote,
            &arm,
            Some("client-a")
        ));
    }
}

#[tokio::test]
async fn pan134_bundle_denial_reports_each_unaired_item() {
    // The monolithic worker is not driven here. Exercise the same denial
    // helper composition; message_bus tests cover the lock-wait race.
    let bus = MessageBus::new(16).unwrap();
    let (_tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
    let (_gw_tx, gw_rx) = bus
        .create_channel(ComponentId::RemoteGateway)
        .await
        .unwrap();
    let (_rig_tx, rig_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(AtomicBool::new(true));
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let items = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "W1ABC K5ARH -10".into(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-a".into()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "K2DEF K5ARH -15".into(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-b".into()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];
    let outcome = bus
        .send_message_if(
            ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Hamlib,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: true,
                }),
                Instant::now(),
            ),
            || tx_send_authorized(crate::message_bus::TxOrigin::Remote, &arm, Some("client-a")),
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendIfOutcome::Denied);
    if outcome == SendIfOutcome::Denied {
        let operator_callsign = tx_denied_operator_attribution(&arm, Some("client-a"));
        for item in &items {
            let denial_reason = format!(
                "arm went stale in the pre-PTT gap: '{}' at {:.0} Hz",
                item.message_text, item.frequency_offset
            );
            emit_disarm_interrupt_signals(
                &bus,
                &audit_log,
                &display_feed_enabled,
                operator_callsign.clone(),
                "denied",
                denial_reason,
                item.qso_id.as_deref(),
            )
            .await;
        }
    }
    assert!(rig_rx.try_recv().is_err(), "denied bundle must not key PTT");
    let events = read_audit_lines(audit_log.path());
    assert_eq!(events.len(), items.len());
    for (item, event) in items.iter().zip(events) {
        let expected = format!(
            "arm went stale in the pre-PTT gap: '{}' at {:.0} Hz",
            item.message_text, item.frequency_offset
        );
        match gw_rx.try_recv().unwrap().message_type {
            MessageType::TxDenied { reason, qso_id } => {
                assert_eq!(reason, expected);
                assert_eq!(qso_id, item.qso_id);
            }
            other => panic!("expected TxDenied, got {other:?}"),
        }
        match tui_rx.try_recv().unwrap().message_type {
            MessageType::DiagnosticEvent { text, qso_id, .. } => {
                assert!(text.starts_with("Remote TX denied ("));
                assert!(!text.contains("disarmed mid-transmission"));
                assert!(text.contains(&expected));
                assert_eq!(qso_id, item.qso_id);
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
        assert_eq!(event.kind, pancetta_agent::audit::AuditKind::TxDenied);
        assert_eq!(event.detail, expected);
        assert_eq!(event.operator_callsign, None);
    }
    assert!(gw_rx.try_recv().is_err());
    assert!(tui_rx.try_recv().is_err());
    let _ = std::fs::remove_file(audit_log.path());
}

#[tokio::test]
async fn pre_ptt_denial_uses_denied_wording_not_mid_transmission_wording() {
    let bus = MessageBus::new(16).unwrap();
    let (_tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
    let (_gw_tx, gw_rx) = bus
        .create_channel(ComponentId::RemoteGateway)
        .await
        .unwrap();
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(AtomicBool::new(true));

    // client-a is armed; the frame is bound to client-b — an identity
    // mismatch, exactly like a stale Step 4b-arm success gone bad in the
    // pre-PTT gap.
    let mut st = pancetta_agent::arm::ArmState::new();
    let now_ms = UtcTimestamp::now().as_millisecond();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "pre-ptt-recheck-jti".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    let denial_reason = "arm went stale in the pre-PTT gap: 'CQ K5ARH EM12' at 1500 Hz".to_string();
    let operator_callsign = tx_denied_operator_attribution(&arm, Some("client-b"));
    emit_disarm_interrupt_signals(
        &bus,
        &audit_log,
        &display_feed_enabled,
        operator_callsign,
        "denied",
        denial_reason.clone(),
        Some("qso-1"),
    )
    .await;

    let tui_msg = tui_rx
        .try_recv()
        .expect("a DiagnosticEvent should have been sent to the Tui channel");
    match tui_msg.message_type {
        MessageType::DiagnosticEvent { text, qso_id, .. } => {
            assert!(
                text.starts_with("Remote TX denied ("),
                "pre-PTT denial must say 'denied', not 'interrupted': {text}"
            );
            assert!(!text.contains("disarmed mid-transmission"));
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }

    let gw_msg = gw_rx
        .try_recv()
        .expect("a TxDenied relay should have been sent to the gateway channel");
    match gw_msg.message_type {
        MessageType::TxDenied { reason, qso_id } => {
            assert_eq!(reason, denial_reason);
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
        }
        other => panic!("expected TxDenied, got {other:?}"),
    }

    let events = read_audit_lines(audit_log.path());
    assert_eq!(
        events.len(),
        1,
        "exactly one TxDenied record must be appended"
    );
    assert_eq!(events[0].kind, pancetta_agent::audit::AuditKind::TxDenied);
    assert_eq!(events[0].detail, denial_reason);
    assert_eq!(
        events[0].operator_callsign, None,
        "an identity-mismatched pre-PTT denial must never attribute to whoever else is armed"
    );
}

#[tokio::test]
async fn mid_flight_interruption_still_uses_interrupted_wording() {
    // Regression guard for the existing AbortedByDisarm call sites:
    // generalizing the helper for the new pre-PTT case must not change
    // the wording an already-on-the-air interruption reports.
    let bus = MessageBus::new(16).unwrap();
    let (_tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(AtomicBool::new(false));
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));

    let operator_callsign = tx_denied_operator_attribution(&arm, None);
    emit_disarm_interrupt_signals(
        &bus,
        &audit_log,
        &display_feed_enabled,
        operator_callsign,
        "interrupted",
        "disarmed mid-transmission: 'CQ K5ARH EM12' at 1500 Hz".to_string(),
        Some("qso-1"),
    )
    .await;

    let tui_msg = tui_rx.try_recv().expect("diagnostic should send");
    match tui_msg.message_type {
        MessageType::DiagnosticEvent { text, .. } => {
            assert!(text.starts_with("Remote TX interrupted ("));
            assert!(text.contains("disarmed mid-transmission"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}
