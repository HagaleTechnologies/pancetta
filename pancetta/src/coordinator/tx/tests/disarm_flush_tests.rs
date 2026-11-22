// PAN-135. Formerly the inline module `pan135_disarm_flush_tests`.
use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

async fn take_flush_ack(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> DeliveryAck {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(message) = rx.try_recv() {
                if let MessageType::AudioFlush { ack, .. } = message.message_type {
                    return ack;
                }
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("audio flush must arrive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_returns_only_after_the_audio_thread_acks() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    let acked_at = Arc::new(std::sync::Mutex::new(None));
    let a2 = acked_at.clone();
    let consumer = tokio::spawn(async move {
        let ack = take_flush_ack(&rx).await;
        sleep(Duration::from_millis(30)).await;
        *a2.lock().unwrap() = Some(Instant::now());
        ack.complete(true);
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let playback_end = tokio::time::Instant::now() + Duration::from_secs(5);
    let out = flush_disarmed_audio_confirmed(&bus, 12_000, playback_end, &shutdown).await;
    let returned = Instant::now();
    consumer.await.unwrap();
    assert_eq!(out, DisarmFlush::Applied);
    assert!(
        returned >= acked_at.lock().unwrap().expect("acked"),
        "never ahead of the ack"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_resends_when_the_ack_is_dropped_in_transit() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    let consumer = tokio::spawn(async move {
        drop(take_flush_ack(&rx).await);
        take_flush_ack(&rx).await.complete(true);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            rx.try_iter().count(),
            0,
            "exactly two flushes, no further retry"
        );
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let playback_end = tokio::time::Instant::now() + Duration::from_secs(5);
    assert_eq!(
        flush_disarmed_audio_confirmed(&bus, 12_000, playback_end, &shutdown).await,
        DisarmFlush::Applied
    );
    consumer.await.unwrap();
}

#[tokio::test]
async fn unacknowledged_flush_waits_out_the_revoked_playback() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, _rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let start = tokio::time::Instant::now();
    let outcome =
        flush_disarmed_audio_confirmed(&bus, 12_000, start + Duration::from_millis(200), &shutdown)
            .await;
    assert_eq!(outcome, DisarmFlush::WaitedOutPlayback);
    assert!(start.elapsed() >= Duration::from_millis(450));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_applied_flush_waits_out_the_revoked_playback() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    let consumer = tokio::spawn(async move {
        take_flush_ack(&rx).await.complete(false);
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let start = tokio::time::Instant::now();
    assert_eq!(
        flush_disarmed_audio_confirmed(
            &bus,
            12_000,
            start + Duration::from_millis(100),
            &shutdown,
        )
        .await,
        DisarmFlush::WaitedOutPlayback
    );
    assert!(start.elapsed() >= Duration::from_millis(350));
    assert!(start.elapsed() < Duration::from_secs(2));
    consumer.await.unwrap();
}

#[tokio::test]
async fn flush_into_a_full_audio_channel_waits_out_the_revoked_playback() {
    let bus = MessageBus::new(1).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    assert!(bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Audio,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .await
        .unwrap());
    let shutdown = Arc::new(AtomicBool::new(false));
    let start = tokio::time::Instant::now();
    assert_eq!(
        flush_disarmed_audio_confirmed(&bus, 12_000, start, &shutdown).await,
        DisarmFlush::WaitedOutPlayback
    );
    assert!(start.elapsed() >= Duration::from_millis(250));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        rx.len(),
        1,
        "all flush attempts dropped; original filler remains"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_interrupts_the_flush_wait() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, _rx) = bus.create_channel(ComponentId::Audio).await.unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let s2 = shutdown.clone();
    let stopper = tokio::spawn(async move {
        sleep(Duration::from_millis(100)).await;
        s2.store(true, Ordering::Release);
    });
    let start = tokio::time::Instant::now();
    assert_eq!(
        flush_disarmed_audio_confirmed(&bus, 12_000, start + Duration::from_secs(10), &shutdown,)
            .await,
        DisarmFlush::Shutdown
    );
    assert!(start.elapsed() < Duration::from_millis(300));
    stopper.await.unwrap();
}
