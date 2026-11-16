use super::*;
use crate::message_bus::MessageBus;

#[tokio::test]
async fn emits_a_warn_diagnostic_the_tui_channel_receives() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    emit_tx_failure_diagnostic(
        &bus,
        Some("qso-42"),
        "K5ARH CQ EM12",
        "encode/modulate error (test)",
    )
    .await;

    let msg = receiver
        .try_recv()
        .expect("a DiagnosticEvent should have been sent to the Tui channel");
    match msg.message_type {
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            qso_id,
            callsign,
        } => {
            assert_eq!(target, "tx.encode");
            assert_eq!(level, pancetta_core::DiagnosticLevel::Warn);
            assert!(text.contains("K5ARH CQ EM12"));
            assert!(text.contains("encode/modulate error (test)"));
            assert_eq!(qso_id.as_deref(), Some("qso-42"));
            assert_eq!(callsign, None);
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn tolerates_a_missing_qso_id() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    emit_tx_failure_diagnostic(&bus, None, "CQ K5ARH EM12", "invalid frequency").await;

    let msg = receiver.try_recv().expect("diagnostic should still send");
    match msg.message_type {
        MessageType::DiagnosticEvent { qso_id, .. } => assert_eq!(qso_id, None),
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn emit_diagnostic_full_carries_wide_attribution() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
    emit_diagnostic_full(
        &bus,
        ComponentId::Qso,
        "qso.security",
        pancetta_core::DiagnosticLevel::Warn,
        "rejected".into(),
        Some("qso-7"),
        Some("W1AW"),
    )
    .await;
    let msg = receiver.try_recv().expect("diagnostic");
    assert_eq!(msg.source, ComponentId::Qso);
    match msg.message_type {
        MessageType::DiagnosticEvent {
            target,
            qso_id,
            callsign,
            ..
        } => {
            assert_eq!(target, "qso.security");
            assert_eq!(qso_id.as_deref(), Some("qso-7"));
            assert_eq!(callsign.as_deref(), Some("W1AW"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn emit_diagnostic_delegates_without_changing_its_own_contract() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
    emit_diagnostic(
        &bus,
        "tx.policy",
        pancetta_core::DiagnosticLevel::Info,
        "held".into(),
        None,
    )
    .await;
    let msg = receiver.try_recv().expect("diagnostic");
    assert_eq!(msg.source, ComponentId::Ft8Transmitter);
    match msg.message_type {
        MessageType::DiagnosticEvent { callsign, .. } => assert_eq!(callsign, None),
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

/// `emit_diagnostic` is the generic sibling wired into the 10
/// `"tx.policy"` sites (observability-diagnostics-plan.md Layer 1) —
/// unlike `emit_tx_failure_diagnostic`, its `target` and `level` are
/// caller-supplied. Pins that both vary correctly and independently.
#[tokio::test]
async fn emit_diagnostic_carries_caller_supplied_target_and_level() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    emit_diagnostic(
        &bus,
        "tx.policy",
        pancetta_core::DiagnosticLevel::Info,
        "dropping stale TX for ended QSO: 'CQ K5ARH EM12'".to_string(),
        Some("qso-7"),
    )
    .await;

    let msg = receiver.try_recv().expect("diagnostic should send");
    match msg.message_type {
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            qso_id,
            ..
        } => {
            assert_eq!(target, "tx.policy");
            assert_eq!(level, pancetta_core::DiagnosticLevel::Info);
            assert!(text.contains("CQ K5ARH EM12"));
            assert_eq!(qso_id.as_deref(), Some("qso-7"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn emit_diagnostic_full_carries_source_and_callsign() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
    emit_diagnostic_full(
        &bus,
        ComponentId::Qso,
        "qso.security",
        pancetta_core::DiagnosticLevel::Warn,
        "rejected frame".into(),
        Some("qso-7"),
        Some("BOGUS9"),
    )
    .await;
    let msg = receiver.try_recv().expect("diagnostic should send");
    assert_eq!(msg.source, ComponentId::Qso);
    match msg.message_type {
        MessageType::DiagnosticEvent {
            target, callsign, ..
        } => {
            assert_eq!(target, "qso.security");
            assert_eq!(callsign.as_deref(), Some("BOGUS9"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}
