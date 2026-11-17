// PAN-290: a poisoned FP filter lock must not panic the Autonomous start, and
// the TX-side gate keeps the stored filter.
use super::fq_f9_placement_feed_when_disabled_tests::build_coordinator;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// PAN-290: a poisoned FP filter lock must not panic the Autonomous
/// start, and the TX-side gate keeps the stored filter (dropping it
/// would fail toward more CQ replies). Plain `#[tokio::test]` runs on
/// the current thread, so the thread-local subscriber below captures
/// what `start_autonomous_component` logs inline.
#[tokio::test]
async fn pan290_poisoned_fp_filter_lock_keeps_tx_side_filter() {
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut c = build_coordinator(true, true).await;
    let filter = pancetta_qso::callsign_continuity::build_filter_with_seed(
        None,
        std::collections::HashSet::new(),
        vec!["W1AW".into()],
        500,
        5,
    )
    .expect("seed-only filter builds");
    *c.fp_filter.write().unwrap() = Some(Arc::new(filter));
    {
        let lock = c.fp_filter.clone();
        let _ = std::thread::spawn(move || {
            let _guard = lock.write().unwrap();
            panic!("PAN-290 test: poisoning the FP filter lock");
        })
        .join();
    }
    assert!(c.fp_filter.is_poisoned());

    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new("pancetta=info,warn"))
        .with_writer(move || writer.clone())
        .finish();
    let started = {
        let _default = tracing::subscriber::set_default(subscriber);
        c.start_autonomous_component().await
    };
    c.shutdown_signal.store(true, Ordering::Release);
    assert!(
        started.is_ok(),
        "a poisoned FP filter lock must not fail the Autonomous start: {started:?}"
    );

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains(
            "FP filter lock poisoned by an earlier panic; keeping the stored filter for TX-side gating"
        ),
        "{text}"
    );
    assert!(
        text.contains("FP filter installed for TX-side gating"),
        "the stored filter must still gate TX: {text}"
    );
}
