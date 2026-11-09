//! PAN-279: the one place a TX frame's outcome is reported.
//!
//! Every frame the TX path takes on ends in exactly one of two settlements:
//! [`settle_aired_frame`] (its audio played) or [`settle_unaired_frame`] (it
//! will not air as itself: dropped, refused, abandoned by a supersede,
//! excluded or folded by the coalescer, or never reached the worker). This
//! module is the only production code that builds
//! `MessageType::TransmitComplete` or `QsoMessage::TxFrameNotSent`, and both
//! halves of one settlement carry the frame's own `TxAttemptId`.
//!
//! Ownership rule (PAN-185 D9, extended to supersedes by PAN-279): Autonomous
//! hears one outcome per QSO frame chain. When a same-QSO successor (a
//! re-key, a requeued superseder, a carried bundle item, a pivot) will report
//! for the QSO, the abandoned frame settles with
//! [`AutonomousOutcome::Withheld`] and only rolls back its own ledger attempt.
//! Both halves travel the durable tier (PAN-185).

use std::collections::HashSet;
use std::time::Instant;

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType, QsoMessage};

/// A frame's identity as both consumers see it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TxFrame {
    pub(super) qso_id: Option<String>,
    pub(super) message_text: String,
    pub(super) attempt_id: Option<pancetta_qso::TxAttemptId>,
}

/// Who tells Autonomous how a QSO's frame ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutonomousOutcome {
    /// No other request will report for this QSO: Autonomous hears this failure.
    Report,
    /// A same-QSO request that is re-keyed, requeued, carried or pivoted to
    /// reports instead (ownership rule, PAN-185 D9, PAN-279).
    Withheld,
}

/// Settle a frame that will not air as itself. Autonomous gets a failed
/// `TransmitComplete` (with the frame's attempt id) only for
/// [`AutonomousOutcome::Report`]; the QSO engine then always gets
/// `TxFrameNotSent` with the frame's attempt when the frame has a `qso_id`.
/// `replaced` is the payload of a frame a pivot replaced (PAN-75). `source`
/// is the sending component (the QSO forwarder addresses itself). Durable:
/// a full channel parks the report, never drops it (PAN-185).
pub(super) async fn settle_unaired_frame(
    bus: &MessageBus,
    source: ComponentId,
    frame: TxFrame,
    replaced: Option<pancetta_qso::UnsentAttemptPayload>,
    autonomous: AutonomousOutcome,
) {
    let TxFrame {
        qso_id,
        message_text,
        attempt_id,
    } = frame;
    if autonomous == AutonomousOutcome::Report {
        send_transmit_complete(
            bus,
            source,
            false,
            message_text.clone(),
            0,
            qso_id.clone(),
            attempt_id,
        )
        .await;
    }
    if let Some(qso_id) = qso_id {
        bus.send_message_durable(ComponentMessage::new(
            source,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                qso_id,
                message_text,
                replaced,
                attempt_id,
            }),
            Instant::now(),
        ))
        .await;
    }
}

/// Settle a frame whose audio played: success to Autonomous, then
/// `TxFrameSent` to the QSO engine when `aired` is known and the frame has
/// a `qso_id` (PAN-182). Durable (PAN-185).
pub(super) async fn settle_aired_frame(
    bus: &MessageBus,
    frame: TxFrame,
    duration_ms: u64,
    aired: Option<pancetta_qso::UnsentAttemptPayload>,
) {
    let TxFrame {
        qso_id,
        message_text,
        attempt_id,
    } = frame;
    send_transmit_complete(
        bus,
        ComponentId::Ft8Transmitter,
        true,
        message_text.clone(),
        duration_ms,
        qso_id.clone(),
        attempt_id,
    )
    .await;
    if let (Some(qso_id), Some(aired)) = (qso_id, aired) {
        bus.send_message_durable(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::TxFrameSent {
                qso_id,
                message_text,
                aired,
                aired_at: chrono::Utc::now(),
            }),
            Instant::now(),
        ))
        .await;
    }
}

async fn send_transmit_complete(
    bus: &MessageBus,
    source: ComponentId,
    success: bool,
    message_text: String,
    duration_ms: u64,
    qso_id: Option<String>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) {
    bus.send_message_durable(ComponentMessage::new(
        source,
        ComponentId::Autonomous,
        MessageType::TransmitComplete {
            success,
            message_text,
            duration_ms,
            qso_id,
            attempt_id,
        },
        Instant::now(),
    ))
    .await;
}

/// The QSO keys (`active_tx_qso_key`) a successor message will air.
pub(super) fn successor_qso_keys(successor: &MessageType) -> HashSet<String> {
    match successor {
        MessageType::TransmitRequest { qso_id, .. } => qso_keys([qso_id.as_deref()]),
        MessageType::MultiTransmitRequest { items, .. } => {
            qso_keys(items.iter().map(|item| item.qso_id.as_deref()))
        }
        _ => HashSet::new(),
    }
}

/// The QSO keys (`active_tx_qso_key`) of a successor's `qso_id`s.
pub(super) fn qso_keys<'a>(qso_ids: impl IntoIterator<Item = Option<&'a str>>) -> HashSet<String> {
    qso_ids
        .into_iter()
        .flatten()
        .map(super::active_tx_qso_key)
        .collect()
}

/// [`AutonomousOutcome::Withheld`] when the successor carries the item's
/// QSO, else [`AutonomousOutcome::Report`] (rule D6, PAN-279).
pub(super) fn autonomous_outcome_for(
    qso_id: Option<&str>,
    successor_keys: &HashSet<String>,
) -> AutonomousOutcome {
    match qso_id {
        Some(id) if successor_keys.contains(&super::active_tx_qso_key(id)) => {
            AutonomousOutcome::Withheld
        }
        _ => AutonomousOutcome::Report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_qso::TxAttemptId;

    fn frame(qso_id: Option<&str>, attempt: Option<u64>) -> TxFrame {
        TxFrame {
            qso_id: qso_id.map(str::to_string),
            message_text: "K1ABC W1AW R-10".into(),
            attempt_id: attempt.map(TxAttemptId),
        }
    }

    fn drain(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> Vec<MessageType> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| m.message_type)
            .collect()
    }

    async fn settle_and_drain(
        f: TxFrame,
        replaced: Option<pancetta_qso::UnsentAttemptPayload>,
        outcome: AutonomousOutcome,
    ) -> (Vec<MessageType>, Vec<MessageType>) {
        let bus = MessageBus::new(16).unwrap();
        let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        settle_unaired_frame(&bus, ComponentId::Ft8Transmitter, f, replaced, outcome).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (drain(&autonomous), drain(&qso))
    }

    #[tokio::test]
    async fn pan279_report_settles_both_consumers_with_one_attempt_id() {
        let (autonomous, qso) = settle_and_drain(
            frame(Some("qso-1"), Some(7)),
            None,
            AutonomousOutcome::Report,
        )
        .await;
        assert_eq!(autonomous.len(), 1);
        assert!(matches!(
            &autonomous[0],
            MessageType::TransmitComplete {
                success: false,
                duration_ms: 0,
                qso_id: Some(id),
                attempt_id: Some(TxAttemptId(7)),
                message_text,
            } if id == "qso-1" && message_text == "K1ABC W1AW R-10"
        ));
        assert_eq!(qso.len(), 1);
        assert!(matches!(
            &qso[0],
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                qso_id,
                replaced: None,
                attempt_id: Some(TxAttemptId(7)),
                ..
            }) if qso_id == "qso-1"
        ));
    }

    #[tokio::test]
    async fn pan279_withheld_settles_only_the_qso_ledger() {
        let (autonomous, qso) = settle_and_drain(
            frame(Some("qso-1"), Some(7)),
            None,
            AutonomousOutcome::Withheld,
        )
        .await;
        assert!(autonomous.is_empty());
        assert_eq!(qso.len(), 1);
        assert!(matches!(
            &qso[0],
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                attempt_id: Some(TxAttemptId(7)),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn pan279_untracked_frame_reports_autonomous_only() {
        let (autonomous, qso) =
            settle_and_drain(frame(None, None), None, AutonomousOutcome::Report).await;
        assert_eq!(autonomous.len(), 1);
        assert!(matches!(
            &autonomous[0],
            MessageType::TransmitComplete {
                success: false,
                qso_id: None,
                ..
            }
        ));
        assert!(qso.is_empty());

        let (autonomous, qso) =
            settle_and_drain(frame(None, None), None, AutonomousOutcome::Withheld).await;
        assert!(autonomous.is_empty());
        assert!(qso.is_empty());
    }

    #[tokio::test]
    async fn pan279_replaced_payload_reaches_the_qso_half() {
        let payload = pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1234.0,
            remote_binding: Some((true, Some("client-a".into()))),
        };
        let (_, qso) = settle_and_drain(
            frame(Some("qso-1"), Some(3)),
            Some(payload.clone()),
            AutonomousOutcome::Withheld,
        )
        .await;
        assert_eq!(qso.len(), 1);
        assert!(matches!(
            &qso[0],
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                replaced: Some(got),
                attempt_id: Some(TxAttemptId(3)),
                ..
            }) if *got == payload
        ));
    }

    #[tokio::test]
    async fn pan279_aired_frame_reports_success_with_its_attempt_id() {
        let bus = MessageBus::new(16).unwrap();
        let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let aired = pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: Some((false, None)),
        };
        settle_aired_frame(
            &bus,
            frame(Some("qso-1"), Some(9)),
            12_640,
            Some(aired.clone()),
        )
        .await;
        settle_aired_frame(&bus, frame(None, None), 12_640, Some(aired.clone())).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let autonomous = drain(&autonomous);
        let qso = drain(&qso);
        assert_eq!(autonomous.len(), 2);
        assert!(matches!(
            &autonomous[0],
            MessageType::TransmitComplete {
                success: true,
                duration_ms: 12_640,
                qso_id: Some(id),
                attempt_id: Some(TxAttemptId(9)),
                ..
            } if id == "qso-1"
        ));
        assert!(matches!(
            &autonomous[1],
            MessageType::TransmitComplete {
                success: true,
                qso_id: None,
                attempt_id: None,
                ..
            }
        ));
        assert_eq!(qso.len(), 1);
        assert!(matches!(
            &qso[0],
            MessageType::QsoMessage(QsoMessage::TxFrameSent { qso_id, aired: got, .. })
                if qso_id == "qso-1" && *got == aired
        ));
    }

    #[tokio::test]
    async fn pan279_settlement_parks_on_full_channels() {
        let bus = MessageBus::new(1).unwrap();
        let autonomous = bus.durable_inbox(ComponentId::Autonomous).await.unwrap();
        let qso = bus.durable_inbox(ComponentId::Qso).await.unwrap();
        let (auto_tx, _) = bus
            .get_or_create_channel(ComponentId::Autonomous)
            .await
            .unwrap();
        let (qso_tx, _) = bus.get_or_create_channel(ComponentId::Qso).await.unwrap();
        auto_tx
            .try_send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Autonomous,
                MessageType::TxStatus { active: false },
                Instant::now(),
            ))
            .unwrap();
        qso_tx
            .try_send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Qso,
                MessageType::TxStatus { active: false },
                Instant::now(),
            ))
            .unwrap();
        settle_unaired_frame(
            &bus,
            ComponentId::Ft8Transmitter,
            frame(Some("qso-1"), Some(7)),
            None,
            AutonomousOutcome::Report,
        )
        .await;
        assert!(matches!(
            autonomous.try_recv().unwrap().message_type,
            MessageType::TxStatus { .. }
        ));
        assert!(matches!(
            autonomous.try_recv().unwrap().message_type,
            MessageType::TransmitComplete {
                success: false,
                attempt_id: Some(TxAttemptId(7)),
                ..
            }
        ));
        assert!(matches!(
            qso.try_recv().unwrap().message_type,
            MessageType::TxStatus { .. }
        ));
        assert!(matches!(
            qso.try_recv().unwrap().message_type,
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                attempt_id: Some(TxAttemptId(7)),
                ..
            })
        ));
        assert_eq!(bus.get_statistics().dropped_messages, 0);
    }

    #[test]
    fn pan279_same_qso_successor_withholds_the_autonomous_outcome() {
        use crate::message_bus::{TransmitRequestItem, TxOrigin};
        let single = |qso_id: Option<&str>| MessageType::TransmitRequest {
            message_text: "K1ABC W1AW RR73".into(),
            frequency_offset: 1500.0,
            tx_parity: None,
            qso_id: qso_id.map(str::to_string),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: None,
        };
        let keys = successor_qso_keys(&single(Some("Qso-1")));
        assert!(keys.contains(&super::super::active_tx_qso_key("qso-1")));
        assert_eq!(
            autonomous_outcome_for(Some("qso-1"), &keys),
            AutonomousOutcome::Withheld
        );
        assert_eq!(
            autonomous_outcome_for(Some("qso-2"), &keys),
            AutonomousOutcome::Report
        );
        assert_eq!(
            autonomous_outcome_for(None, &keys),
            AutonomousOutcome::Report
        );

        let keys = successor_qso_keys(&single(None));
        assert!(keys.is_empty());
        assert_eq!(
            autonomous_outcome_for(Some("qso-1"), &keys),
            AutonomousOutcome::Report
        );

        let item = |qso_id: Option<&str>| TransmitRequestItem {
            message_text: "X".into(),
            frequency_offset: 1500.0,
            qso_id: qso_id.map(str::to_string),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: None,
        };
        let multi = MessageType::MultiTransmitRequest {
            items: vec![item(Some("qso-1")), item(Some("QSO-2")), item(None)],
            tx_parity: None,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        };
        let keys = successor_qso_keys(&multi);
        assert_eq!(keys.len(), 2);
        assert_eq!(
            autonomous_outcome_for(Some("qso-2"), &keys),
            AutonomousOutcome::Withheld
        );
        assert_eq!(
            autonomous_outcome_for(Some("qso-3"), &keys),
            AutonomousOutcome::Report
        );
    }
}
