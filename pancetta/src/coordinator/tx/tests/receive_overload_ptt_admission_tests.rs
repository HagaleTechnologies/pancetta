use super::*;

#[tokio::test]
async fn gain_reservation_refuses_every_ptt_origin_but_always_allows_unkey() {
    let bus = MessageBus::new(8).unwrap();
    let (_, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    bus.receive_gain.confirmed_off(0);
    assert!(bus.receive_gain.reserve());
    for source in [ComponentId::Ft8Transmitter, ComponentId::Tui] {
        assert_eq!(
            send_ptt_command(&bus, source, true, || true),
            SendIfOutcome::Denied
        );
        assert_eq!(
            send_ptt_command(&bus, source, false, || true),
            SendIfOutcome::Enqueued
        );
        assert!(matches!(
            rx.try_recv().unwrap().message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false })
        ));
    }
    assert!(rx.try_recv().is_err());
    bus.receive_gain.fail_gain();
    assert_eq!(
        send_ptt_command(&bus, ComponentId::Tui, true, || true),
        SendIfOutcome::Denied
    );
    assert_eq!(
        send_ptt_command(&bus, ComponentId::Tui, false, || true),
        SendIfOutcome::Enqueued
    );
}
