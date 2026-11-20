use super::*;
use pancetta_qso::TxAttemptId;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

const CHANNELS: [ComponentId; 4] = [
    ComponentId::Hamlib,
    ComponentId::Tui,
    ComponentId::Autonomous,
    ComponentId::Qso,
];

/// A bus with every channel an aborted sleep step sends on.
struct Station {
    bus: MessageBus,
    rxs: Vec<crossbeam_channel::Receiver<ComponentMessage>>,
}

impl Station {
    async fn new() -> Self {
        let bus = MessageBus::new(64).unwrap();
        let mut rxs = Vec::new();
        for id in CHANNELS {
            rxs.push(bus.create_channel(id).await.unwrap().1);
        }
        Self { bus, rxs }
    }

    /// Every payload received, per channel (in `CHANNELS` order).
    fn drain(&self) -> Vec<Vec<String>> {
        self.rxs
            .iter()
            .map(|rx| {
                std::iter::from_fn(|| rx.try_recv().ok())
                    .map(|m| format!("{:?}", m.message_type))
                    .collect()
            })
            .collect()
    }
}

/// The worker state one `on_sleep_outcome` call borrows.
struct World {
    station: Station,
    _audit_tmp: crate::coordinator::audit_test_util::TempPath,
    audit_log: pancetta_agent::audit::AuditLog,
    display_feed_enabled: Arc<AtomicBool>,
    arm: Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    guard: PttGuard,
    pivoted_once: HashMap<String, (String, f64)>,
    pivoted_from: PivotReplaced,
    shutdown: Arc<AtomicBool>,
}

impl World {
    async fn new() -> Self {
        let station = Station::new().await;
        let guard = PttGuard::new(
            station.bus.clone(),
            Arc::new(AtomicBool::new(false)),
            &Arc::new(tokio::sync::Mutex::new(())),
            &Arc::new(AtomicU64::new(0)),
            &Arc::new(AtomicU64::new(0)),
            &PendingPttOff::default(),
        )
        .await;
        let audit_tmp =
            crate::coordinator::audit_test_util::TempPath::new("pancetta-tx-sleep-outcome");
        Self {
            station,
            audit_log: pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
            _audit_tmp: audit_tmp,
            display_feed_enabled: Arc::new(AtomicBool::new(false)),
            arm: Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new())),
            guard,
            pivoted_once: HashMap::new(),
            pivoted_from: PivotReplaced::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn ctx(&mut self) -> AbortCtx<'_> {
        AbortCtx {
            message_bus: &self.station.bus,
            audit_log: &self.audit_log,
            display_feed_enabled: &self.display_feed_enabled,
            remote_tx_arm: &self.arm,
            remote_client_key_id: None,
            ptt_guard: &mut self.guard,
            pivoted_once: &mut self.pivoted_once,
            pivoted_from: &mut self.pivoted_from,
            shutdown: &self.shutdown,
            sample_rate: 12_000,
        }
    }

    /// Records a Step 4b/4c pivot of `qso_id` that replaced [`replaced`].
    fn pivot(&mut self, qso_id: &str) -> String {
        let key = super::super::active_tx_qso_key(qso_id);
        self.pivoted_once
            .insert(key.clone(), ("K1ABC W9XYZ R-05".to_string(), 1500.0));
        self.pivoted_from.insert(key.clone(), replaced());
        key
    }
}

fn replaced() -> ReplacedFrame {
    (
        "K1ABC W9XYZ -10".to_string(),
        pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: None,
        },
        Some(TxAttemptId(7)),
    )
}

fn item(text: &str, qso_id: &str, attempt: u64) -> crate::message_bus::TransmitRequestItem {
    crate::message_bus::TransmitRequestItem {
        attempt_id: Some(TxAttemptId(attempt)),
        message_text: text.to_string(),
        frequency_offset: 1500.0,
        qso_id: Some(qso_id.to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

fn is_ptt_off(payload: &str) -> bool {
    payload
        == format!(
            "{:?}",
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false })
        )
}

#[test]
fn abort_log_lines_are_byte_identical() {
    let playback = SleepPhase::Playback {
        playback_end: tokio::time::Instant::now(),
    };
    let cases = [
        (
            TxShape::Single,
            SleepPhase::PreSlot,
            SleepAbort::Shutdown,
            "TX aborted between PTT and slot by shutdown",
        ),
        (
            TxShape::Single,
            SleepPhase::PreSlot,
            SleepAbort::Operator,
            "TX aborted between PTT and slot by operator (F8)",
        ),
        (
            TxShape::Single,
            SleepPhase::PreSlot,
            SleepAbort::Disarm,
            "TX aborted between PTT and slot: remote arm no longer permits it",
        ),
        (
            TxShape::Single,
            playback,
            SleepAbort::Shutdown,
            "TX aborted during playback by shutdown",
        ),
        (
            TxShape::Single,
            playback,
            SleepAbort::Operator,
            "TX aborted during playback by operator (F8)",
        ),
        (
            TxShape::Single,
            playback,
            SleepAbort::Disarm,
            "TX aborted during playback: remote arm no longer permits it",
        ),
        (
            TxShape::Bundle,
            SleepPhase::PreSlot,
            SleepAbort::Shutdown,
            "Multi-TX aborted between PTT and slot by shutdown",
        ),
        (
            TxShape::Bundle,
            SleepPhase::PreSlot,
            SleepAbort::Operator,
            "Multi-TX aborted between PTT and slot by operator (F8)",
        ),
        (
            TxShape::Bundle,
            SleepPhase::PreSlot,
            SleepAbort::Disarm,
            "Multi-TX aborted between PTT and slot: remote arm no longer permits it",
        ),
        (
            TxShape::Bundle,
            playback,
            SleepAbort::Shutdown,
            "Multi-TX aborted during playback by shutdown",
        ),
        (
            TxShape::Bundle,
            playback,
            SleepAbort::Operator,
            "Multi-TX aborted during playback by operator (F8)",
        ),
        (
            TxShape::Bundle,
            playback,
            SleepAbort::Disarm,
            "Multi-TX aborted during playback: remote arm no longer permits it",
        ),
    ];
    for (shape, phase, abort, line) in cases {
        assert_eq!(
            sleep_abort_log_line(shape, phase, abort),
            line,
            "{shape:?} {phase:?} {abort:?}"
        );
    }
}

#[tokio::test]
async fn preslot_disarm_unwinds_the_single_frame_pivot() {
    let mut world = World::new().await;
    let key = world.pivot("qso-1");
    let mut pivoted_this_key = Some(key.clone());
    let flow = on_sleep_outcome(
        SleepOutcome::AbortedByDisarm,
        SleepPhase::PreSlot,
        SleepInFlight::Single {
            message_text: "K1ABC W9XYZ R-05",
            frequency_offset: 1500.0,
            qso_id: Some("qso-1"),
            attempt_id: Some(TxAttemptId(9)),
            pivoted_this_key: &mut pivoted_this_key,
        },
        world.ctx(),
    )
    .await;
    assert!(matches!(
        flow,
        ControlFlow::Break(SleepBreak::Exit(TxExit::NextRequest))
    ));
    assert!(pivoted_this_key.is_none());
    assert!(!world.pivoted_once.contains_key(&key));
    assert!(!world.pivoted_from.contains_key(&key));
    assert!(
        !world.guard.armed,
        "PTT-off delivered stands the guard down"
    );
    let got = world.station.drain();
    assert_eq!(got[0].len(), 1);
    assert!(is_ptt_off(&got[0][0]), "Hamlib got {:?}", got[0]);

    let want = Station::new().await;
    report_unaired_frame(
        &want.bus,
        "K1ABC W9XYZ R-05".to_string(),
        Some("qso-1".to_string()),
        Some(TxAttemptId(9)),
        Some(replaced()),
    )
    .await
    .unwrap();
    let want = want.drain();
    assert_eq!(
        got[2..],
        want[2..],
        "Autonomous and Qso settle the replaced frame"
    );
    assert!(
        got[3].iter().any(|m| m.contains("K1ABC W9XYZ -10")),
        "the QSO's rollback names the replaced frame: {:?}",
        got[3]
    );
}

#[tokio::test]
async fn preslot_disarm_unwinds_bundle_pivots_per_item() {
    let mut world = World::new().await;
    let key = world.pivot("qso-1");
    let items = vec![
        item("K1ABC W9XYZ R-05", "qso-1", 9),
        item("K2DEF W9XYZ RR73", "qso-2", 11),
    ];
    let keys = vec![key.clone()];
    let flow = on_sleep_outcome(
        SleepOutcome::AbortedByDisarm,
        SleepPhase::PreSlot,
        SleepInFlight::Bundle {
            items: &items,
            pivoted_this_bundle_keys: &keys,
        },
        world.ctx(),
    )
    .await;
    assert!(matches!(
        flow,
        ControlFlow::Break(SleepBreak::Exit(TxExit::NextRequest))
    ));
    assert!(!world.pivoted_once.contains_key(&key));
    assert!(!world.pivoted_from.contains_key(&key));
    assert!(!world.guard.armed);
    let got = world.station.drain();
    assert_eq!(got[0].len(), 1);
    assert!(is_ptt_off(&got[0][0]), "Hamlib got {:?}", got[0]);

    let want = Station::new().await;
    for (item, pivot) in items.iter().zip([Some(replaced()), None]) {
        report_unaired_frame(
            &want.bus,
            item.message_text.clone(),
            item.qso_id.clone(),
            item.attempt_id,
            pivot,
        )
        .await
        .unwrap();
    }
    let want = want.drain();
    assert_eq!(
        got[2..],
        want[2..],
        "only the pivoted item carries the replaced frame"
    );
}

#[tokio::test]
async fn completed_continues_and_sends_nothing() {
    let mut world = World::new().await;
    let items = vec![item("K1ABC W9XYZ R-05", "qso-1", 9)];
    for phase in [
        SleepPhase::PreSlot,
        SleepPhase::Playback {
            playback_end: tokio::time::Instant::now(),
        },
    ] {
        let mut pivoted_this_key = None;
        let flow = on_sleep_outcome(
            SleepOutcome::Completed,
            phase,
            SleepInFlight::Single {
                message_text: "K1ABC W9XYZ R-05",
                frequency_offset: 1500.0,
                qso_id: Some("qso-1"),
                attempt_id: Some(TxAttemptId(9)),
                pivoted_this_key: &mut pivoted_this_key,
            },
            world.ctx(),
        )
        .await;
        assert!(matches!(flow, ControlFlow::Continue(())));
        let flow = on_sleep_outcome(
            SleepOutcome::Completed,
            phase,
            SleepInFlight::Bundle {
                items: &items,
                pivoted_this_bundle_keys: &[],
            },
            world.ctx(),
        )
        .await;
        assert!(matches!(flow, ControlFlow::Continue(())));
    }
    assert!(world.station.drain().iter().all(Vec::is_empty));
    assert!(world.guard.armed, "the guard stays armed");
}

#[tokio::test]
async fn superseded_is_handed_back_unchanged() {
    let mut world = World::new().await;
    let message = || MessageType::TxQueueStatus {
        sending: None,
        queued: Vec::new(),
    };
    let mut pivoted_this_key = None;
    let flow = on_sleep_outcome(
        SleepOutcome::Superseded(message()),
        SleepPhase::PreSlot,
        SleepInFlight::Single {
            message_text: "K1ABC W9XYZ R-05",
            frequency_offset: 1500.0,
            qso_id: Some("qso-1"),
            attempt_id: Some(TxAttemptId(9)),
            pivoted_this_key: &mut pivoted_this_key,
        },
        world.ctx(),
    )
    .await;
    match flow {
        ControlFlow::Break(SleepBreak::Superseded(got)) => {
            assert_eq!(format!("{got:?}"), format!("{:?}", message()));
        }
        ControlFlow::Break(SleepBreak::Exit(exit)) => {
            panic!("expected Superseded, got {exit:?}")
        }
        ControlFlow::Continue(()) => panic!("expected Superseded, got Continue"),
    }
    assert!(world.station.drain().iter().all(Vec::is_empty));
    assert!(world.guard.armed);
}
