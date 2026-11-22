use super::*;
use pancetta_qso::TxAttemptId;
use std::ops::ControlFlow;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

type Frame = (String, Option<String>, Option<TxAttemptId>);

fn frame(text: &str, qso_id: &str, attempt: u64) -> Frame {
    (
        text.to_string(),
        Some(qso_id.to_string()),
        Some(TxAttemptId(attempt)),
    )
}

fn flag(set: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(set))
}

fn forward_jump() -> pancetta_core::slot_clock::WallClockJump {
    pancetta_core::slot_clock::WallClockJump::Forward(SignedDuration::from_secs(5))
}

/// A bus with the three channels Step 4 sends on.
struct Station {
    bus: MessageBus,
    tui: crossbeam_channel::Receiver<ComponentMessage>,
    autonomous: crossbeam_channel::Receiver<ComponentMessage>,
    qso: crossbeam_channel::Receiver<ComponentMessage>,
}

/// Everything one [`Station`] received, per channel, in arrival order.
struct Sent {
    tui: Vec<ComponentMessage>,
    autonomous: Vec<ComponentMessage>,
    qso: Vec<ComponentMessage>,
}

impl Station {
    async fn new() -> Self {
        let bus = MessageBus::new(16).unwrap();
        let (_, tui) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        Self {
            bus,
            tui,
            autonomous,
            qso,
        }
    }

    fn drain(&self) -> Sent {
        let all = |rx: &crossbeam_channel::Receiver<ComponentMessage>| {
            std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>()
        };
        Sent {
            tui: all(&self.tui),
            autonomous: all(&self.autonomous),
            qso: all(&self.qso),
        }
    }
}

impl Sent {
    /// The payloads, per channel, for comparing two stations.
    fn payloads(&self) -> [Vec<String>; 3] {
        let debug = |msgs: &[ComponentMessage]| {
            msgs.iter()
                .map(|m| format!("{:?}", m.message_type))
                .collect::<Vec<_>>()
        };
        [debug(&self.tui), debug(&self.autonomous), debug(&self.qso)]
    }

    fn is_empty(&self) -> bool {
        self.tui.is_empty() && self.autonomous.is_empty() && self.qso.is_empty()
    }

    /// Every Autonomous message as a failed `TransmitComplete`.
    fn failed_completes(&self) -> Vec<Frame> {
        self.autonomous
            .iter()
            .map(|m| match &m.message_type {
                MessageType::TransmitComplete {
                    success: false,
                    message_text,
                    duration_ms: 0,
                    qso_id,
                    attempt_id,
                } => (message_text.clone(), qso_id.clone(), *attempt_id),
                other => panic!("expected a failed TransmitComplete, got {other:?}"),
            })
            .collect()
    }

    /// Every Qso message as a `TxFrameNotSent` with nothing replaced.
    fn frames_not_sent(&self) -> Vec<Frame> {
        self.qso
            .iter()
            .map(|m| match &m.message_type {
                MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                    qso_id,
                    message_text,
                    replaced: None,
                    attempt_id,
                }) => (message_text.clone(), Some(qso_id.clone()), *attempt_id),
                other => panic!("expected TxFrameNotSent, got {other:?}"),
            })
            .collect()
    }
}

fn is_strip_clear(message: &ComponentMessage) -> bool {
    matches!(
        &message.message_type,
        MessageType::TxQueueStatus { sending: None, queued } if queued.is_empty()
    )
}

fn never_built() -> Vec<Frame> {
    panic!("frames must not be built for this outcome")
}

/// Runs `on_ptt_wait` for a clock jump on one bus and
/// `drop_frames_for_clock_jump` on another, and checks they sent the
/// same thing.
async fn check_clock_jump(shape: TxShape, frames: Vec<Frame>) {
    let got = Station::new().await;
    let flow = on_ptt_wait(
        PttWait::ClockJumped(forward_jump()),
        shape,
        || frames.clone(),
        &got.bus,
        &flag(false),
    )
    .await;
    assert_eq!(flow, ControlFlow::Break(TxExit::NextRequest));
    let want = Station::new().await;
    drop_frames_for_clock_jump(
        &want.bus,
        &frames,
        forward_jump(),
        &mut PivotReplaced::new(),
    )
    .await;
    let (got, want) = (got.drain(), want.drain());
    assert_eq!(got.payloads(), want.payloads());
    // One diagnostic per frame, then the strip clear.
    assert_eq!(got.tui.len(), frames.len() + 1);
    assert!(got.tui[..frames.len()].iter().all(|m| matches!(
        &m.message_type,
        MessageType::DiagnosticEvent { text, .. } if text.starts_with("dropping stale TX")
    )));
    assert!(is_strip_clear(&got.tui[frames.len()]));
    assert_eq!(got.failed_completes(), frames);
    assert_eq!(got.frames_not_sent(), frames);
}

#[tokio::test]
async fn clock_jump_settles_the_single_frame_like_drop_frames_for_clock_jump() {
    check_clock_jump(TxShape::Single, vec![frame("K1ABC N0CALL FN20", "q1", 7)]).await;
}

#[tokio::test]
async fn clock_jump_settles_every_bundle_frame() {
    check_clock_jump(
        TxShape::Bundle,
        vec![
            frame("K1ABC N0CALL FN20", "q1", 1),
            frame("W9XYZ N0CALL -07", "q2", 2),
        ],
    )
    .await;
}

#[tokio::test]
async fn operator_abort_clears_the_strip_only_for_a_single_frame() {
    let single = Station::new().await;
    let frames = vec![frame("K1ABC N0CALL FN20", "q1", 3)];
    let flow = on_ptt_wait(
        PttWait::Interrupted,
        TxShape::Single,
        || frames.clone(),
        &single.bus,
        &flag(false),
    )
    .await;
    assert_eq!(flow, ControlFlow::Break(TxExit::NextRequest));
    let sent = single.drain();
    assert_eq!(sent.tui.len(), 1);
    assert!(is_strip_clear(&sent.tui[0]));
    assert_eq!(sent.failed_completes(), frames);
    assert_eq!(sent.frames_not_sent(), frames);
    assert!(
        sent.tui[0].id < sent.autonomous[0].id && sent.tui[0].id < sent.qso[0].id,
        "the strip clears before the frame settles"
    );

    let bundle = Station::new().await;
    let frames = vec![
        frame("K1ABC N0CALL FN20", "q1", 1),
        frame("W9XYZ N0CALL -07", "q2", 2),
    ];
    let flow = on_ptt_wait(
        PttWait::Interrupted,
        TxShape::Bundle,
        || frames.clone(),
        &bundle.bus,
        &flag(false),
    )
    .await;
    assert_eq!(flow, ControlFlow::Break(TxExit::NextRequest));
    let sent = bundle.drain();
    assert!(
        sent.tui.is_empty(),
        "a bundle F8 abort sends the TUI nothing"
    );
    assert_eq!(sent.failed_completes(), frames);
    assert_eq!(sent.frames_not_sent(), frames);
}

#[tokio::test]
async fn shutdown_interrupt_stops_the_worker_and_sends_nothing() {
    for shape in [TxShape::Single, TxShape::Bundle] {
        let station = Station::new().await;
        let flow = on_ptt_wait(
            PttWait::Interrupted,
            shape,
            never_built,
            &station.bus,
            &flag(true),
        )
        .await;
        assert_eq!(flow, ControlFlow::Break(TxExit::StopWorker), "{shape:?}");
        assert!(station.drain().is_empty(), "{shape:?}");
    }
}

#[tokio::test]
async fn reached_continues_without_building_frames() {
    for shape in [TxShape::Single, TxShape::Bundle] {
        let station = Station::new().await;
        let flow = on_ptt_wait(
            PttWait::Reached,
            shape,
            never_built,
            &station.bus,
            &flag(false),
        )
        .await;
        assert_eq!(flow, ControlFlow::Continue(()), "{shape:?}");
        assert!(station.drain().is_empty(), "{shape:?}");
    }
}
