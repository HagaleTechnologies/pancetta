//! PAN-358: the queued-call TTL task runs an expiry pass as soon as the
//! slot-clock driver publishes a clock event, without waiting for its 15 s
//! tick.

use super::*;
use jiff::SignedDuration;
use pancetta_core::slot_clock::ClockEvent;
use pancetta_core::time::UtcTimestamp;

fn queued(name: &str, queued_at: ClockStamp) -> PendingManualCall {
    PendingManualCall {
        callsign: name.to_string(),
        frequency_hz: 1500.0,
        dx_parity: Some(pancetta_core::slot::SlotParity::Even),
        queued_at,
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 0,
        hold_mode: false,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    }
}

fn forward_event() -> ClockEvent {
    ClockEvent {
        epoch: 1,
        skew: SignedDuration::from_secs(60),
        observed_at: UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 1, 5).unwrap(),
        slot_ns: pancetta_core::slot::SLOT_NS,
        resync: true,
    }
}

#[tokio::test]
async fn ttl_task_runs_a_pass_on_a_clock_event() {
    let bus = MessageBus::new(1000).expect("bus");
    let (_tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
    let pending: PendingManualCalls =
        Arc::new(Mutex::new(std::collections::VecDeque::from([queued(
            "JA1AAA",
            test_queue_clock().stamp(),
        )])));
    let (events_tx, events_rx) = tokio::sync::watch::channel(None::<ClockEvent>);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = spawn_queued_call_ttl(
        pending.clone(),
        bus.clone(),
        test_queue_clock(),
        events_rx,
        shutdown.clone(),
    );

    // The interval's first tick runs one pass at once; the fresh call stays.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(pending.lock().await.len(), 1, "a fresh call is kept");

    // The machine slept (or the clock stepped): the call is now 10 min 30 s
    // old on the wall clock. The next 15 s tick is more than 14 s away.
    let now = test_queue_clock().stamp();
    pending.lock().await[0].queued_at = ClockStamp {
        mono: now.mono,
        wall: now.wall - SignedDuration::from_secs(630),
    };
    events_tx.send_replace(Some(forward_event()));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !pending.lock().await.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the clock event must run an expiry pass before the next 15 s tick"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let mut saw_status = false;
    while let Ok(msg) = tui_rx.try_recv() {
        if let MessageType::StatusUpdate { ref text, .. } = msg.message_type {
            if text.contains("Queued call to JA1AAA expired") {
                saw_status = true;
            }
        }
    }
    assert!(saw_status, "the expiry status line reaches the TUI");

    shutdown.store(true, Ordering::Release);
    events_tx.send_replace(Some(forward_event()));
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the task exits on the first wake after shutdown")
        .expect("the task does not panic");
}

#[tokio::test]
async fn ttl_task_keeps_ticking_after_the_event_sender_is_dropped() {
    let bus = MessageBus::new(1000).expect("bus");
    let pending: PendingManualCalls = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let (events_tx, events_rx) = tokio::sync::watch::channel(None::<ClockEvent>);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = spawn_queued_call_ttl(pending, bus, test_queue_clock(), events_rx, shutdown);
    drop(events_tx);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !task.is_finished(),
        "a closed event channel leaves the 15 s tick running"
    );
    task.abort();
}
