// PAN-290: the decoder's per-window FP-filter read must survive a poisoned
// lock. Before the fix, the unwrapped read panicked on every window after a
// writer panic, and every supervisor restart of the decoder shared the same
// poisoned lock.
use super::*;
use std::collections::HashSet;

const BYPASS_WARNING: &str =
    "FP filter lock poisoned by an earlier panic; decoding this window with the filter bypassed";

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
impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn built_filter() -> Arc<pancetta_qso::CallsignContinuityFilter> {
    Arc::new(
        pancetta_qso::callsign_continuity::build_filter_with_seed(
            None,
            HashSet::new(),
            vec!["W1AW".into()],
            500,
            5,
        )
        .expect("seed-only filter builds"),
    )
}

/// Poison `lock` the way a real writer panic would: panic on another
/// thread while holding the write guard.
fn poison(lock: &Arc<FpFilterLock>) {
    let held = lock.clone();
    let _ = std::thread::spawn(move || {
        let _guard = held.write().unwrap();
        panic!("PAN-290 test: poisoning the FP filter lock");
    })
    .join();
    assert!(lock.is_poisoned(), "the lock must be poisoned by the panic");
}

/// Run `fp_filter_for_window` under the production default log filter
/// and return what it read and what it logged.
fn read_with_logs(
    lock: &FpFilterLock,
) -> (Option<Arc<pancetta_qso::CallsignContinuityFilter>>, String) {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new("pancetta=info,warn"))
        .with_writer(move || writer.clone())
        .finish();
    let read = tracing::subscriber::with_default(subscriber, || fp_filter_for_window(lock));
    (read, logs.text())
}

#[test]
fn poisoned_lock_bypasses_the_stored_filter_and_warns() {
    let lock: Arc<FpFilterLock> = Arc::new(std::sync::RwLock::new(Some(built_filter())));
    poison(&lock);

    // The stored filter is still there behind the poison flag...
    assert!(lock
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some());

    // ...but the decoder's read bypasses it and warns, every window.
    for window in 1..=2 {
        let (read, logs) = read_with_logs(&lock);
        assert!(
            read.is_none(),
            "window {window}: a poisoned lock must bypass the filter"
        );
        assert!(logs.contains("WARN"), "window {window}: {logs}");
        assert!(
            logs.contains("pancetta::fp_filter"),
            "window {window}: {logs}"
        );
        assert_eq!(
            logs.matches(BYPASS_WARNING).count(),
            1,
            "window {window}: exactly one warning per window: {logs}"
        );
    }
    assert!(lock.is_poisoned(), "the decoder read never clears poison");
}

#[test]
fn healthy_lock_returns_the_stored_filter_without_warning() {
    let stored = built_filter();
    let lock: FpFilterLock = std::sync::RwLock::new(Some(stored.clone()));
    let (read, logs) = read_with_logs(&lock);
    let read = read.expect("a healthy lock returns the stored filter");
    assert!(Arc::ptr_eq(&read, &stored), "same filter, not a copy");
    assert!(!logs.contains("poisoned"), "{logs}");

    let unset: FpFilterLock = std::sync::RwLock::new(None);
    let (read, logs) = read_with_logs(&unset);
    assert!(read.is_none(), "an unset lock passes decodes unfiltered");
    assert!(!logs.contains("poisoned"), "{logs}");
}
