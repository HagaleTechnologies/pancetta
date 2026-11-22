//! PAN-274: the ADIF subscriber writes every completion it still holds when
//! coordinator shutdown asks it to finish, reports a lag as an error, and
//! shutdown's wait for it is bounded.
use super::{start_adif_subscriber, AdifSubscribers};
use pancetta_qso::{QsoEvent, QsoState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

const CALLS: [&str; 6] = ["A1AAA", "B1BBB", "C1CCC", "D1DDD", "E1EEE", "K1DEF"];
const LAG_LINE: &str =
    "ADIF subscriber lagged by 3 QSO events; any QSO completed among them is not in qsos.adi";

fn completed(call: &str) -> QsoEvent {
    let now = pancetta_core::time::UtcTimestamp::now();
    let qso_id = pancetta_qso::QsoId::new_v4();
    QsoEvent::QsoCompleted {
        qso_id,
        metadata: pancetta_qso::QsoMetadata {
            qso_id,
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some(call.to_string()),
            frequency: 14_074_000.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: now,
            end_time: Some(now),
            reports: pancetta_qso::states::SignalReports::default(),
            grids: pancetta_qso::states::GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: std::collections::HashMap::new(),
            adif_field_types: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            tx_parity_provisional: false,
            initiated_by: pancetta_qso::states::CallInitiation::Auto,
            role: pancetta_qso::states::QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin: false,
            remote_client_key_id: None,
        },
        state_history: vec![],
        messages: vec![],
    }
}

fn state_changed() -> QsoEvent {
    QsoEvent::StateChanged {
        qso_id: pancetta_qso::QsoId::new_v4(),
        old_state: QsoState::Idle,
        new_state: QsoState::Idle,
        timestamp: pancetta_core::time::UtcTimestamp::now(),
    }
}

/// Which of the test callsigns `path` holds, in file order.
fn calls_in(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut found: Vec<(usize, &str)> = CALLS
        .iter()
        .filter_map(|call| text.find(call).map(|at| (at, *call)))
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(_, call)| call.to_string())
        .collect()
}

async fn writer(path: &std::path::Path) -> Arc<pancetta_qso::AdifLogWriter> {
    Arc::new(pancetta_qso::AdifLogWriter::open(path).await.unwrap())
}

/// Collects formatted log output so a test can assert on it.
#[derive(Clone)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }
}

/// Capture every log line on this thread (the current-thread test runtime
/// runs the spawned subscriber here too) until the guard drops.
fn capture_logs() -> (Capture, tracing::subscriber::DefaultGuard) {
    let capture = Capture(Arc::new(std::sync::Mutex::new(Vec::new())));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (capture, guard)
}

/// A channel of capacity 2 that has lagged by 3: five completions were
/// sent, so only D1DDD and E1EEE are still retained.
fn lagged_channel() -> (broadcast::Sender<QsoEvent>, broadcast::Receiver<QsoEvent>) {
    let (tx, rx) = broadcast::channel(2);
    for call in &CALLS[..5] {
        tx.send(completed(call)).unwrap();
    }
    (tx, rx)
}

#[tokio::test]
async fn adif_subscriber_writes_every_completion_queued_when_asked_to_finish() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    let w = writer(&path).await;
    let (tx, rx) = broadcast::channel(16);
    for event in [
        state_changed(),
        completed("A1AAA"),
        state_changed(),
        completed("B1BBB"),
    ] {
        tx.send(event).unwrap();
    }
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(w, rx, &subs));
    subs.stop(Duration::from_secs(2)).await;
    assert_eq!(calls_in(&path), vec!["A1AAA", "B1BBB"]);
}

#[tokio::test]
async fn adif_subscriber_writes_completions_sent_while_it_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    let w = writer(&path).await;
    let (tx, rx) = broadcast::channel(16);
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(w, rx, &subs));
    // Let it park in `recv()`.
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(state_changed()).unwrap();
    tx.send(completed("K1DEF")).unwrap();
    subs.stop(Duration::from_secs(2)).await;
    assert_eq!(calls_in(&path), vec!["K1DEF"]);
}

#[tokio::test]
async fn adif_subscriber_writes_the_retained_completions_after_a_lag_during_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    let w = writer(&path).await;
    let (logs, _guard) = capture_logs();
    let (_tx, rx) = lagged_channel();
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(w, rx, &subs));
    // Stop is requested before the subscriber's first poll.
    subs.stop(Duration::from_secs(2)).await;
    assert_eq!(calls_in(&path), vec!["D1DDD", "E1EEE"]);
    let logs = logs.text();
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(logs.contains(LAG_LINE), "{logs}");
}

#[tokio::test]
async fn adif_subscriber_reports_a_lag_as_an_error_and_keeps_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    let w = writer(&path).await;
    let (logs, _guard) = capture_logs();
    let (_tx, rx) = lagged_channel();
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(w, rx, &subs));
    for _ in 0..100 {
        if calls_in(&path).len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(calls_in(&path), vec!["D1DDD", "E1EEE"]);
    let logs = logs.text();
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(logs.contains(LAG_LINE), "{logs}");
    subs.stop(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn adif_shutdown_wait_is_bounded_while_the_logbook_is_locked() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    // Open first: the open writes the header under the lock.
    let w = writer(&path).await;
    let lock = pancetta_qso::logbook_lock::LogbookLock::open(&path).unwrap();
    let _held = lock.try_lock().unwrap().expect("the logbook lock is free");
    let (logs, _guard) = capture_logs();
    let (tx, rx) = broadcast::channel(16);
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(w, rx, &subs));
    tx.send(completed("K1DEF")).unwrap();
    let started = std::time::Instant::now();
    subs.stop(Duration::from_millis(300)).await;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "stop took {took:?}");
    let logs = logs.text();
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(
        logs.contains(
            "qsos.adi writer still busy after 300ms; shutdown continues, and a QSO it \
             had not written yet is not in qsos.adi"
        ),
        "{logs}"
    );
}

/// The supervisor-restart case: two subscribers on one tracker.
#[tokio::test]
async fn adif_subscribers_wait_for_every_tracked_subscriber() {
    let tmp = tempfile::tempdir().unwrap();
    let first_path = tmp.path().join("first.adi");
    let second_path = tmp.path().join("second.adi");
    let first = writer(&first_path).await;
    let second = writer(&second_path).await;
    let (first_tx, first_rx) = broadcast::channel(16);
    let (second_tx, second_rx) = broadcast::channel(16);
    first_tx.send(completed("A1AAA")).unwrap();
    second_tx.send(completed("B1BBB")).unwrap();
    let subs = AdifSubscribers::new();
    subs.track(start_adif_subscriber(first, first_rx, &subs));
    subs.track(start_adif_subscriber(second, second_rx, &subs));
    subs.stop(Duration::from_secs(2)).await;
    assert_eq!(calls_in(&first_path), vec!["A1AAA"]);
    assert_eq!(calls_in(&second_path), vec!["B1BBB"]);
}

#[tokio::test]
async fn adif_subscribers_forget_subscribers_that_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    let w = writer(&path).await;
    let subs = AdifSubscribers::new();
    let (closed_tx, closed_rx) = broadcast::channel::<QsoEvent>(4);
    drop(closed_tx);
    // Its channel is closed, so it exits on its own.
    let ended = start_adif_subscriber(w.clone(), closed_rx, &subs);
    for _ in 0..100 {
        if ended.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ended.is_finished(), "a closed channel ends the subscriber");
    subs.track(ended);
    assert_eq!(subs.tracked(), 1);
    let (_tx, rx) = broadcast::channel(4);
    subs.track(start_adif_subscriber(w, rx, &subs));
    assert_eq!(subs.tracked(), 1, "the ended subscriber was forgotten");
    subs.stop(Duration::from_secs(2)).await;
}
