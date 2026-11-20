use super::*;

#[tokio::test]
async fn proceeds_when_no_cq_attempt_id_to_register() {
    let bus = MessageBus::new(1000).expect("bus");
    assert!(
        register_autonomous_cq_open_or_decline(&bus, pancetta_qso::QsoId::new_v4(), None).await,
        "nothing to register (not a self-CQ) must always proceed"
    );
}

#[tokio::test]
async fn proceeds_when_the_registration_is_delivered() {
    let bus = MessageBus::new(1000).expect("bus");
    // Register the destination channel so the AutonomousCqOpened send
    // actually has somewhere to land.
    bus.create_channel(ComponentId::Autonomous)
        .await
        .expect("create channel");
    assert!(
        register_autonomous_cq_open_or_decline(&bus, pancetta_qso::QsoId::new_v4(), Some(7)).await,
        "a successfully delivered registration must proceed to dispatch"
    );
}

/// PAN-45 (Codex round 6 on PR #301) regression: before this fix, a
/// dropped `AutonomousCqOpened` send was swallowed by `send_message`
/// into `Ok(())` and the caller dispatched `start_cq_with_id` anyway --
/// opening a real QSO the autonomous operator never learned the
/// qso_id<->attempt_id association for. A later failed
/// `TransmitComplete` for that qso_id would then find no
/// `pending_self_cq_qsos` entry and be silently ignored, leaving the
/// failed CQ's streak/offset mutation uncorrected forever.
#[tokio::test]
async fn declines_when_the_registration_is_dropped() {
    // No channel registered for ComponentId::Autonomous at all --
    // `send_message_checked` hits its "no channel found for
    // destination" branch and returns `Ok(false)`, deterministically
    // simulating a dropped delivery without racing a real bounded
    // queue.
    let bus = MessageBus::new(1000).expect("bus");
    assert!(
        !register_autonomous_cq_open_or_decline(&bus, pancetta_qso::QsoId::new_v4(), Some(7)).await,
        "a dropped registration must decline to dispatch, not proceed as if it \
         succeeded -- the caller's start_cq_with_id must never run in this case"
    );
}
