use super::pan115_worker_gate_tests::{await_ptt_on, request, worker_harness, Harness};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, RigControlMessage};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// 20m: in-band under the default region-2 plan (as the PAN-118 tests).
const IN_BAND_DIAL_HZ: u64 = 14_074_000;

fn is_ptt_off(msg: &ComponentMessage) -> bool {
    matches!(
        msg.message_type,
        MessageType::RigControl(RigControlMessage::SetPtt { state: false })
    )
}

async fn hamlib_drops(h: &Harness) -> u32 {
    h.coordinator
        .message_bus
        .get_component_health()
        .await
        .into_iter()
        .find(|c| c.component_id == ComponentId::Hamlib)
        .map(|c| c.error_count)
        .expect("the harness registers Hamlib")
}

async fn request_tune(h: &Harness, duration_secs: u32) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

/// Ticket scenario 1's setup and catch-up, after PTT-on: fill Hamlib's
/// channel, wait for the worker's first PTT-off attempt to be dropped
/// on it, then let Hamlib catch up (drain) and count the PTT-offs that
/// reach it from then on.
async fn ptt_offs_after_hamlib_catches_up(h: &Harness, within: Duration) -> usize {
    let bus = &h.coordinator.message_bus;
    let mut filled = 0usize;
    loop {
        let enqueued = bus
            .send_message_checked(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Hamlib,
                MessageType::AudioData(Vec::new()),
                Instant::now(),
            ))
            .await
            .unwrap();
        if !enqueued {
            break;
        }
        filled += 1;
        assert!(filled <= 100_000, "Hamlib's channel never filled");
    }
    let full_at = hamlib_drops(h).await;
    let deadline = Instant::now() + within;
    while hamlib_drops(h).await == full_at {
        assert!(
            Instant::now() < deadline,
            "the worker never tried to unkey into the full channel"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Hamlib catches up. A retry can land while this drains, so count
    // PTT-offs in the backlog as well as after it.
    let mut offs = h.hamlib_rx.try_iter().filter(is_ptt_off).count();
    tokio::time::sleep(Duration::from_millis(400)).await;
    offs += h.hamlib_rx.try_iter().filter(is_ptt_off).count();
    offs
}

/// Ticket scenario 1 on the tune path (keys at once, no slot wait).
/// Before PAN-273 this delivered 0 PTT-offs: the dropped send returned
/// `Ok(())` and the guard was disarmed anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tune_unkey_retries_a_dropped_ptt_off_until_hamlib_has_room() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    request_tune(&h, 2).await;
    assert!(await_ptt_on(&h, Duration::from_secs(5)).await, "tune keyed");
    let offs = ptt_offs_after_hamlib_catches_up(&h, Duration::from_secs(10)).await;
    assert_eq!(offs, 1, "the worker's retry delivers exactly one unkey");
}

/// Ticket scenario 2: a delivered PTT-off stands the guard down, so its
/// `Drop` sends no second unkey and the keyed flag clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tune_unkey_on_a_healthy_channel_sends_exactly_one_ptt_off() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    request_tune(&h, 1).await;
    assert!(await_ptt_on(&h, Duration::from_secs(5)).await, "tune keyed");
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let offs = h.hamlib_rx.try_iter().filter(is_ptt_off).count();
    assert_eq!(offs, 1, "one unkey, no duplicate from the guard's Drop");
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// Ticket scenario 1 as written: a frame has finished playing and the
/// worker unkeys at Step 9 into a full Hamlib channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_tx_unkey_retries_a_dropped_ptt_off_after_the_frame_plays() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    request(&h, "CQ N0CALL FN20").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "the frame keyed"
    );
    let offs = ptt_offs_after_hamlib_catches_up(&h, Duration::from_secs(40)).await;
    assert_eq!(offs, 1, "the worker's retry delivers exactly one unkey");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = None;
    while completed.is_none() && Instant::now() < deadline {
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if message_text == "CQ N0CALL FN20" {
                    completed = Some(success);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(completed.is_some(), "the frame's TransmitComplete arrives");
}

/// Source pin for the two clock-step PTT hold guards (PAN-114), which
/// need a backward wall-clock step and have no runtime seam: no unkey in
/// this file builds its own PTT-off for a best-effort `send_message`.
/// The needle is built at runtime so this test does not match itself.
#[test]
fn pan273_no_best_effort_ptt_off_remains_in_tx_rs() {
    let source = include_str!("../../tx.rs");
    let needle = ["let ", "ptt_off_msg"].concat();
    assert!(
        !source.contains(&needle),
        "every TX-worker unkey goes through PttGuard::release (PAN-273)"
    );
    for context in [
        "Step 9 unkey",
        "Multi-TX Step 9 unkey",
        "Tune unkey",
        "Supersede",
        "Clock-step PTT hold guard",
    ] {
        let call = [".release(\"", context, "\")"].concat();
        assert!(source.contains(&call), "missing {call}");
    }
}
