//! PAN-357: the real Hamlib component and TX worker key a serial RTS line
//! (fake, through the `test_ptt_line_device` seam) and never CAT.

use super::ptt_line_tests::FakeLines;
use super::*;
use crate::coordinator::hamlib::HamlibReconnectRequest;
use crate::coordinator::ApplicationCoordinator;
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, TxOrigin};
use pancetta_config::Config;
use pancetta_hamlib::{MockRig, MockRigConfig};
use std::sync::atomic::Ordering;
use std::time::Instant;

const DEV: &str = "/dev/fake-ptt";

async fn new_coordinator() -> (ApplicationCoordinator, tempfile::TempDir) {
    let scratch = tempfile::tempdir().unwrap();
    let c = ApplicationCoordinator::new(
        Config::default(),
        None,
        true,
        true,
        None,
        None,
        None,
        1500.0,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
        scratch.path().join("config.toml"),
        true,
        Some(scratch.path().to_path_buf()),
    )
    .await
    .unwrap();
    (c, scratch)
}

fn mock_rig() -> Arc<MockRig> {
    Arc::new(MockRig::new(MockRigConfig {
        connection_delay_ms: 0,
        operation_delay_ms: 0,
        ..Default::default()
    }))
}

/// A coordinator whose Hamlib component keys `fake` on [`DEV`], with a
/// `MockRig` (connected by the Hamlib start) as the inner rig.
async fn started_on(fake: &FakeLines) -> (ApplicationCoordinator, Arc<MockRig>, tempfile::TempDir) {
    let (mut c, scratch) = new_coordinator().await;
    let rig = mock_rig();
    c.test_mock_rig = Some(rig.clone());
    c.ptt_line = fake.line();
    c.test_ptt_line_device = Some(DEV.into());
    c.start_hamlib_component().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !c.hamlib_command_loop_ready.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "Hamlib command loop ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (c, rig, scratch)
}

/// Polls until the last RTS write is `level`, checking at every poll that the
/// inner rig was never keyed by CAT. Returns when it was seen.
async fn wait_rts(fake: &FakeLines, rig: &MockRig, level: bool, budget: Duration) -> Instant {
    let deadline = Instant::now() + budget;
    loop {
        assert_ne!(
            rig.get_ptt(Vfo::Current).await.ok(),
            Some(PttState::On),
            "the inner rig must never be keyed by CAT"
        );
        if fake.rts_levels().last() == Some(&level) && (level || fake.rts_levels().contains(&true))
        {
            return Instant::now();
        }
        assert!(Instant::now() < deadline, "RTS must become {level}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn manual_ptt(c: &ApplicationCoordinator, on: bool) {
    let outcome =
        crate::coordinator::tx::send_ptt_command(&c.message_bus, ComponentId::Tui, on, || true);
    assert_eq!(outcome, crate::message_bus::SendIfOutcome::Enqueued);
}

#[tokio::test]
async fn tx_frame_holds_rts_for_the_frame_and_drops_it_on_unkey() {
    let fake = FakeLines::default();
    let (mut c, rig, _scratch) = started_on(&fake).await;
    for component in [
        ComponentId::Autonomous,
        ComponentId::Qso,
        ComponentId::Tui,
        ComponentId::Audio,
    ] {
        c.message_bus
            .get_or_create_channel(component)
            .await
            .unwrap();
    }
    c.start_transmitter_component().await.unwrap();
    c.message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "CQ N0CALL FN20".into(),
                frequency_offset: 1500.0,
                qso_id: None,
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();

    let keyed_at = wait_rts(&fake, &rig, true, Duration::from_secs(40)).await;
    assert!(
        c.ptt_active.load(Ordering::Acquire),
        "ptt_active while RTS is high"
    );
    // Hold for the frame: still high (and ptt_active) part-way through.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(fake.rts_levels().last(), Some(&true), "RTS held mid-frame");
    assert!(c.ptt_active.load(Ordering::Acquire));

    let unkeyed_at = wait_rts(&fake, &rig, false, Duration::from_secs(30)).await;
    let held = unkeyed_at - keyed_at;
    assert!(
        held >= Duration::from_secs(12),
        "RTS must cover the 12.64 s FT8 frame, held {held:?}"
    );
    assert_eq!(
        fake.rts_levels().iter().filter(|level| **level).count(),
        1,
        "one key-down for one frame"
    );
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn manual_ptt_command_raises_and_lowers_rts() {
    let fake = FakeLines::default();
    let (c, rig, _scratch) = started_on(&fake).await;
    manual_ptt(&c, true);
    wait_rts(&fake, &rig, true, Duration::from_secs(5)).await;
    manual_ptt(&c, false);
    wait_rts(&fake, &rig, false, Duration::from_secs(5)).await;
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn teardown_lowers_rts() {
    let fake = FakeLines::default();
    let (mut c, rig, _scratch) = started_on(&fake).await;
    manual_ptt(&c, true);
    wait_rts(&fake, &rig, true, Duration::from_secs(5)).await;
    c.teardown_hamlib().await;
    assert_eq!(fake.rts_levels().last(), Some(&false));
    assert!(!c.ptt_line.is_asserted());
    assert_eq!(rig.get_ptt(Vfo::Current).await.ok(), Some(PttState::Off));
}

#[tokio::test]
async fn shutdown_releases_rts_without_the_hamlib_loop() {
    let fake = FakeLines::default();
    let (mut c, _scratch) = new_coordinator().await;
    let line = fake.line();
    c.ptt_line = line.clone();
    line.configure(Some(DEV.into()), "/dev/fake-cat")
        .await
        .unwrap();
    line.set(true).await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&true));

    c.shutdown().await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&false));
    assert!(line.set(true).await.is_err(), "latched after shutdown");
    assert_eq!(fake.rts_levels().last(), Some(&false));
}

#[tokio::test]
async fn hamlib_restart_reuses_the_open_line() {
    let fake = FakeLines::default();
    let (mut c, rig, _scratch) = started_on(&fake).await;
    assert_eq!(fake.opens(), 1);
    let (respond, result) = tokio::sync::oneshot::channel();
    c.handle_hamlib_reconnect_request(HamlibReconnectRequest { respond })
        .await;
    result.await.unwrap().unwrap();
    assert_eq!(fake.opens(), 1, "a restart keeps the open line");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !c.hamlib_command_loop_ready.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "Hamlib command loop ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    manual_ptt(&c, true);
    wait_rts(&fake, &rig, true, Duration::from_secs(5)).await;
    manual_ptt(&c, false);
    wait_rts(&fake, &rig, false, Duration::from_secs(5)).await;
    assert_eq!(fake.opens(), 1);
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_open_is_reported_and_retried_on_key_down() {
    let fake = FakeLines::default();
    fake.fail_opens.store(1, Ordering::Release);
    let (c, rig, _scratch) = {
        let (mut c, scratch) = new_coordinator().await;
        let rig = mock_rig();
        c.test_mock_rig = Some(rig.clone());
        c.ptt_line = fake.line();
        c.test_ptt_line_device = Some(DEV.into());
        let (_tx, tui) = c
            .message_bus
            .get_or_create_channel(ComponentId::Tui)
            .await
            .unwrap();
        c.start_hamlib_component().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !c.hamlib_command_loop_ready.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "Hamlib command loop ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut reported = None;
        while let Ok(message) = tui.try_recv() {
            if let MessageType::Error {
                component_id: ComponentId::Hamlib,
                error_message,
                ..
            } = message.message_type
            {
                if error_message.contains(DEV) {
                    reported = Some(error_message);
                }
            }
        }
        let reported = reported.expect("the failed open reaches the TUI as a Hamlib error");
        assert!(
            reported.starts_with(&format!("Serial PTT: cannot open serial PTT port {DEV}: ")),
            "{reported}"
        );
        assert!(
            reported.ends_with("TX will not key until the port opens; each key-down retries it."),
            "{reported}"
        );
        (c, rig, scratch)
    };
    assert_eq!(fake.opens(), 0);

    manual_ptt(&c, true);
    wait_rts(&fake, &rig, true, Duration::from_secs(5)).await;
    assert_eq!(fake.opens(), 1, "the key-down retried the open");
    manual_ptt(&c, false);
    wait_rts(&fake, &rig, false, Duration::from_secs(5)).await;
    c.shutdown().await.unwrap();
}
