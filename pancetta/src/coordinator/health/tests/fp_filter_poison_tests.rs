// PAN-290: a poisoned FP filter lock must not kill the real FT8 decoder.
use super::supervisor_tests::test_coordinator;
use crate::message_bus::ComponentId;

/// PAN-290: a poisoned FP filter lock must not kill the real FT8
/// decoder. Before the fix the decoder's `fp_filter.read().unwrap()`
/// panicked on its first window, and every supervisor restart shared
/// the same poisoned lock, so decoding stopped for good. The decoder
/// takes window N+1 off the bounded channel only after window N has
/// passed the filter step, so three drained windows prove it kept
/// decoding with the filter bypassed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan290_poisoned_fp_filter_lock_does_not_kill_decoder() {
    let mut coordinator = test_coordinator().await;
    {
        let lock = coordinator.fp_filter.clone();
        let _ = std::thread::spawn(move || {
            let _guard = lock.write().unwrap();
            panic!("PAN-290 test: poisoning the FP filter lock");
        })
        .join();
    }
    assert!(coordinator.fp_filter.is_poisoned());

    coordinator.init_decode_handles();
    crate::coordinator::pipeline::register_decode_bus_channels(&coordinator.message_bus)
        .await
        .unwrap();
    coordinator.start_ft8_pipeline().await.unwrap();

    let to_decoder = coordinator.decode_handles().unwrap().dsp_to_ft8_tx.clone();
    for window in 1..=3 {
        to_decoder
            .send(crate::coordinator::pipeline::DecodeWindow {
                samples: vec![0.0; 180_000],
                dial_hz: 14_074_000,
                sweep: None,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !to_decoder.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "decoder stopped taking windows at window {window}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    assert!(
        coordinator
            .named_task_handles
            .iter()
            .find(|(id, _)| *id == ComponentId::Ft8Decoder)
            .is_some_and(|(_, handle)| !handle.is_finished()),
        "the decoder task must still be running"
    );
    assert!(
        coordinator.fp_filter.is_poisoned(),
        "the decoder bypasses the filter; it does not repair the lock"
    );
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}
