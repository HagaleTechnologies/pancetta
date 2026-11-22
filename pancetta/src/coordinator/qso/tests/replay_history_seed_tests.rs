use super::super::ApplicationCoordinator;
use pancetta_config::Config;
use pancetta_qso::async_database::QsoDatabase;
use pancetta_qso::priority::WorkedStationLookup;
use pancetta_qso::{GridSquares, QsoMetadata, QsoProgress, QsoState, SignalReports};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 14.074 MHz — a real 20m FT8 frequency; `is_duplicate` and the seeding
/// code both resolve this to the "20M" band via their own (different,
/// but for this frequency equivalent) frequency→band helpers.
const FREQ_20M_HZ: f64 = 14_074_000.0;

async fn test_coordinator_with_home(home: &std::path::Path) -> ApplicationCoordinator {
    let config = Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true,                                                // no_audio
        true,                                                // headless
        None,                                                // no WAV
        Some(std::path::PathBuf::from("/some/capture/dir")), // --replay
        None,                                                // no test-tx
        1500.0,
        shutdown,
        Vec::new(),                                             // no config warnings
        std::env::temp_dir().join("pancetta-test-config.toml"), // test-only config path
        true,                                                   // test-only: assume TOML
        None,                                                   // PAN-270: no session-home override
    )
    .await
    .expect("coordinator creation should succeed");
    coordinator.pancetta_home_override = Some(home.to_path_buf());
    coordinator
}

fn sample_progress(callsign: &str) -> QsoProgress {
    QsoProgress {
        state: QsoState::Idle,
        state_history: vec![],
        messages: vec![],
        metadata: QsoMetadata {
            qso_id: uuid::Uuid::new_v4(),
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some(callsign.to_string()),
            frequency: FREQ_20M_HZ,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: chrono::Utc::now(),
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
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
            tx_parity_provisional: false,
        },
    }
}

/// The seeding work runs inside `start_qso_component`'s own spawned
/// task, not before it returns -- poll for up to ~5s (mirrors
/// `replay_local_log_tests::wait_for_adif_record`'s same rationale)
/// rather than assuming it's synchronous.
async fn wait_for_duplicate(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: &str,
    freq_hz: f64,
) -> bool {
    for _ in 0..100 {
        if lookup.is_duplicate(callsign, freq_hz) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    lookup.is_duplicate(callsign, freq_hz)
}

/// A LIVE (non-replay) coordinator rooted at `home`, station call W1ABC
/// (the call `replay_local_log_tests::complete_one_qso` works as).
async fn live_coordinator_with_home(home: &std::path::Path) -> ApplicationCoordinator {
    let mut config = Config::default();
    config.station.callsign = "W1ABC".to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true, // no_audio
        true, // headless
        None, // no WAV
        None, // live, not --replay
        None, // no test-tx
        1500.0,
        shutdown,
        Vec::new(),
        std::env::temp_dir().join("pancetta-test-config.toml"),
        true,
        None, // PAN-270: no session-home override
    )
    .await
    .expect("coordinator creation should succeed");
    coordinator.pancetta_home_override = Some(home.to_path_buf());
    coordinator
}

/// PAN-57: the pending-upload worker is owned once per process. A Qso
/// restart re-runs `start_qso_component`, which must reuse the worker
/// rather than start a second writer on `upload_outbox.json`. With an
/// empty queue nothing is sent and no queue file is created.
#[tokio::test]
async fn upload_outbox_is_started_once_across_qso_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = live_coordinator_with_home(tmp.path()).await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.network.clublog.enabled = true;
        cfg.network.clublog.email = "op@example.com".to_string();
        cfg.network.clublog.password = "dummy".to_string();
        cfg.network.clublog.api_key = "dummy".to_string();
    }
    coordinator.start_qso_component().await.unwrap();
    let first = coordinator
        .upload_outbox
        .clone()
        .expect("ClubLog enabled on a live run starts the worker");
    assert_eq!(
        first.destinations(),
        &[pancetta_dx::upload_outbox::UploadDestination::ClubLog]
    );
    coordinator.start_qso_component().await.unwrap();
    let second = coordinator.upload_outbox.clone().unwrap();
    assert!(
        first.same_worker(&second),
        "a Qso restart reuses the worker"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!tmp
        .path()
        .join(".pancetta")
        .join(pancetta_dx::upload_outbox::OUTBOX_FILE_NAME)
        .exists());
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-57: coordinator shutdown waits for the upload worker, so a batch
/// it has taken is saved before the runtime can cancel the task.
#[tokio::test]
async fn coordinator_shutdown_waits_for_the_upload_worker_to_save() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = live_coordinator_with_home(tmp.path()).await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.network.clublog.enabled = true;
        cfg.network.clublog.email = "op@example.com".to_string();
        cfg.network.clublog.password = "dummy".to_string();
        cfg.network.clublog.api_key = "dummy".to_string();
    }
    coordinator.start_qso_component().await.unwrap();
    let handle = coordinator.upload_outbox.clone().unwrap();
    // Flag first, so the worker never sends (no network); the batch is
    // still in the channel when shutdown starts.
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
    handle
        .enqueue(vec![pancetta_dx::upload_outbox::NewUpload {
            qso_id: uuid::Uuid::new_v4(),
            destination: pancetta_dx::upload_outbox::UploadDestination::ClubLog,
            call: "K1ABC".to_string(),
            payload: "<CALL:5>K1ABC<EOR>".to_string(),
        }])
        .await;
    coordinator.shutdown().await.unwrap();
    // Shutdown returned only after the worker finished, not while it is
    // still in its grace window.
    assert!(
        handle.worker_stopped(),
        "shutdown did not wait for the worker"
    );
    drop(handle);
    let path = tmp
        .path()
        .join(".pancetta")
        .join(pancetta_dx::upload_outbox::OUTBOX_FILE_NAME);
    let (store, _) = pancetta_dx::upload_outbox::OutboxStore::open(path);
    assert_eq!(store.len(), 1, "batch taken before shutdown was saved");
    assert_eq!(store.entries()[0].call, "K1ABC");
}

/// A QRZ XML endpoint that accepts every connection and never answers. The
/// receiver gets one `()` per accepted connection. It is tokio's channel,
/// because the test runtime is current-thread and a blocking recv would
/// stall it.
fn silent_qrz_server() -> (String, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
            let _ = tx.send(());
        }
    });
    (format!("http://{addr}/xml/current/"), rx)
}

/// Records every payload it is given and accepts it.
struct RecordingTransport(std::sync::Mutex<Vec<String>>);

impl super::super::upload_outbox::OutboxTransport for RecordingTransport {
    fn destinations(&self) -> Vec<pancetta_dx::upload_outbox::UploadDestination> {
        vec![pancetta_dx::upload_outbox::UploadDestination::ClubLog]
    }

    async fn deliver(
        &self,
        _d: pancetta_dx::upload_outbox::UploadDestination,
        payload: String,
    ) -> super::super::upload_outbox::Delivery {
        self.0.lock().unwrap().push(payload);
        super::super::upload_outbox::Delivery::Accepted {
            duplicate: false,
            remote_id: None,
        }
    }
}

/// `live_coordinator_with_home` with ClubLog enabled (dummy credentials).
async fn clublog_coordinator(home: &std::path::Path) -> ApplicationCoordinator {
    let coordinator = live_coordinator_with_home(home).await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.network.clublog.enabled = true;
        cfg.network.clublog.email = "op@example.com".to_string();
        cfg.network.clublog.password = "dummy".to_string();
        cfg.network.clublog.api_key = "dummy".to_string();
    }
    coordinator
}

fn outbox_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pancetta")
        .join(pancetta_dx::upload_outbox::OUTBOX_FILE_NAME)
}

/// PAN-424: start the QSO component and wait until its task reaches the
/// receiver loop. `qso_consumer_ready` is released only after the local
/// writers and the upload subscriber have started, so a QSO completed
/// after this returns has live subscribers. The fixed 500 ms sleep this
/// replaces raced a slow startup on a loaded CI runner (PR #683, run
/// 44019998471): shutdown aborted the QSO task before it had spawned the
/// upload subscriber, so the completion was never queued.
async fn start_qso_component_and_wait_until_ready(coordinator: &mut ApplicationCoordinator) {
    let mut ready = coordinator.qso_consumer_ready.subscribe();
    coordinator.start_qso_component().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        ready.wait_for(|ready| *ready),
    )
    .await
    .expect("QSO consumer never became ready")
    .expect("qso_consumer_ready sender dropped");
}

/// How long [`wait_for_queued_upload`] waits for the worker.
const UPLOAD_QUEUED_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// PAN-424: bounded poll until the upload worker holds work for
/// `destination`. The worker publishes its health only after adding a
/// batch and saving the queue, so a non-empty entry means the upload is
/// queued on disk. Returns false on timeout so the caller can still shut
/// the coordinator down before failing.
async fn wait_for_queued_upload(
    health: &std::sync::Arc<std::sync::Mutex<super::super::upload_outbox::UploadHealth>>,
    destination: pancetta_dx::upload_outbox::UploadDestination,
) -> bool {
    let queued = || {
        health
            .lock()
            .unwrap()
            .destinations
            .iter()
            .any(|d| d.destination == destination && d.pending + d.held > 0)
    };
    tokio::time::timeout(UPLOAD_QUEUED_WAIT, async {
        while !queued() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// PAN-232 Tier 1: a QSO that completes while its QRZ XML lookup hangs
/// still has its uploads queued when pancetta shuts down, without the
/// QRZ data, shutdown does not wait for the lookup, and the next start
/// sends them.
#[tokio::test]
async fn qso_completed_during_a_hung_qrz_lookup_is_queued_at_shutdown_and_sent_at_next_start() {
    let tmp = tempfile::tempdir().unwrap();
    let (qrz_url, mut qrz_connections) = silent_qrz_server();
    let mut coordinator = clublog_coordinator(tmp.path()).await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.network.qrz_xml.enabled = true;
        cfg.network.qrz_xml.username = "user".to_string();
        cfg.network.qrz_xml.password = "pass".to_string();
    }
    coordinator.qrz_xml_base_url_override = Some(qrz_url);
    coordinator.start_qso_component().await.unwrap();
    // Let the writers and the upload subscriber subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");
    super::replay_local_log_tests::complete_one_qso(&manager).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), qrz_connections.recv())
        .await
        .expect("the QRZ lookup for K1DEF started")
        .expect("server thread alive");

    let started = std::time::Instant::now();
    coordinator.shutdown().await.unwrap();
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(2),
        "shutdown waited for the QRZ lookup: {took:?}"
    );

    let path = outbox_path(tmp.path());
    let (store, _) = pancetta_dx::upload_outbox::OutboxStore::open(path.clone());
    assert_eq!(store.len(), 1, "the QSO's ClubLog upload was not queued");
    let entry = &store.entries()[0];
    assert_eq!(entry.call, "K1DEF");
    assert_eq!(
        entry.destination,
        pancetta_dx::upload_outbox::UploadDestination::ClubLog
    );
    assert!(
        !entry.payload.contains("<STATE:"),
        "queued with QRZ data it never got: {}",
        entry.payload
    );
    drop(store);

    // The next start sends it.
    let transport = Arc::new(RecordingTransport(std::sync::Mutex::new(Vec::new())));
    let timing = super::super::upload_outbox::OutboxTiming {
        slow_retry: std::time::Duration::from_millis(200),
        drain_spacing: std::time::Duration::from_millis(5),
        tick: std::time::Duration::from_millis(5),
        shutdown_grace: std::time::Duration::from_millis(100),
        persist_retry: std::time::Duration::from_millis(5),
        persist_retry_cap: std::time::Duration::from_millis(20),
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let (handle, task) = super::super::upload_outbox::start_upload_outbox(
        path.clone(),
        transport.clone(),
        pancetta_dx::upload_outbox::RetrySchedule::new(3, 10, 40, 2.0, 0.0, timing.slow_retry),
        timing,
        Arc::new(std::sync::Mutex::new(
            super::super::upload_outbox::UploadHealth::default(),
        )),
        None,
        shutdown.clone(),
        None,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while transport.0.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the queued upload was not sent at the next start"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    shutdown.store(true, std::sync::atomic::Ordering::Release);
    super::super::upload_outbox::stop_upload_outbox(Some(handle), Some(task), timing).await;
    let sent = transport.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("K1DEF"), "{}", sent[0]);
    let (store, _) = pancetta_dx::upload_outbox::OutboxStore::open(path);
    assert_eq!(store.len(), 0, "delivered upload left in the queue");
}

/// PAN-232 Tier 2: with uploads enabled and nothing in flight, the worker
/// closes its queue as soon as the upload subscriber lets go, instead of
/// waiting out a fixed grace window.
#[tokio::test]
async fn shutdown_closes_the_upload_queue_at_once_when_no_qso_is_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = clublog_coordinator(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    coordinator.shutdown().await.unwrap();
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(1),
        "shutdown waited out the upload worker's grace window: {took:?}"
    );
}

/// PAN-232 guard: the QSO component can still complete a QSO after the
/// shutdown flag flips (it finishes the message it is handling), so the
/// upload subscriber must keep receiving until shutdown asks it to
/// finish, not stop on the flag alone.
#[tokio::test]
async fn qso_completed_after_the_shutdown_flag_still_has_its_uploads_queued() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = clublog_coordinator(tmp.path()).await;
    start_qso_component_and_wait_until_ready(&mut coordinator).await;
    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
    // More than four of the subscriber's shutdown polls.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    super::replay_local_log_tests::complete_one_qso(&manager).await;
    // The subscriber queued the completion it received after the flag
    // flipped. Observed, not slept past; checked again on disk below.
    let queued = wait_for_queued_upload(
        &coordinator.upload_health,
        pancetta_dx::upload_outbox::UploadDestination::ClubLog,
    )
    .await;
    coordinator.shutdown().await.unwrap();
    assert!(
        queued,
        "the QSO's ClubLog upload was not queued within {UPLOAD_QUEUED_WAIT:?} of completing"
    );
    let (store, _) = pancetta_dx::upload_outbox::OutboxStore::open(outbox_path(tmp.path()));
    assert_eq!(store.len(), 1, "the QSO's ClubLog upload was not queued");
    assert_eq!(store.entries()[0].call, "K1DEF");
}

/// PAN-274: a QSO that completes after the shutdown flag flips (the Qso
/// task finishes the message it is handling) is in qsos.adi when shutdown
/// returns, and the upload queue holds the same QSO.
#[tokio::test]
async fn qso_completed_after_the_shutdown_flag_reaches_qsos_adi_and_the_upload_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = clublog_coordinator(tmp.path()).await;
    start_qso_component_and_wait_until_ready(&mut coordinator).await;
    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    super::replay_local_log_tests::complete_one_qso(&manager).await;
    coordinator.shutdown().await.unwrap();

    let adif_path = tmp.path().join(".pancetta").join("qsos.adi");
    let adif = std::fs::read_to_string(&adif_path).unwrap_or_default();
    assert_eq!(
        super::replay_local_log_tests::adif_record_count(&adif_path),
        1,
        "{adif}"
    );
    assert!(adif.contains("K1DEF"), "{adif}");
    let (store, _) = pancetta_dx::upload_outbox::OutboxStore::open(outbox_path(tmp.path()));
    assert_eq!(store.len(), 1, "the QSO's ClubLog upload was not queued");
    assert_eq!(store.entries()[0].call, "K1DEF");
}

/// PAN-274: the same with no upload destination: the logbook does not
/// depend on uploads being enabled, and shutdown stays fast.
#[tokio::test]
async fn qso_completed_after_the_shutdown_flag_reaches_qsos_adi_without_uploads() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = live_coordinator_with_home(tmp.path()).await;
    start_qso_component_and_wait_until_ready(&mut coordinator).await;
    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    super::replay_local_log_tests::complete_one_qso(&manager).await;
    let started = std::time::Instant::now();
    coordinator.shutdown().await.unwrap();
    let took = started.elapsed();

    let adif_path = tmp.path().join(".pancetta").join("qsos.adi");
    let adif = std::fs::read_to_string(&adif_path).unwrap_or_default();
    assert_eq!(
        super::replay_local_log_tests::adif_record_count(&adif_path),
        1,
        "{adif}"
    );
    assert!(adif.contains("K1DEF"), "{adif}");
    assert!(
        took < std::time::Duration::from_secs(2),
        "shutdown took {took:?}"
    );
}

/// sophon 2026-10-07 / 2026-10-15: the SQLite logger opened `qso.db`
/// BEFORE the startup "index older than ADIF" rebuild deleted and
/// recreated that file, so its pooled connection kept pointing at the
/// deleted file (its immediate startup backup failed "malformed" / "not
/// a database", and QSOs it logged landed in the unlinked inode). The
/// logger must open the index only after the rebuild.
#[tokio::test]
async fn live_startup_rebuild_happens_before_the_logger_opens_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let pancetta_dir = tmp.path().join(".pancetta");
    tokio::fs::create_dir_all(&pancetta_dir).await.unwrap();
    let db_path = pancetta_dir.join("qso.db");
    {
        let db = QsoDatabase::open(&db_path).await.unwrap();
        db.insert_qso(&sample_progress("W9STALE")).await.unwrap();
    }
    // ADIF contacts differ from the index -> the rebuild path.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    tokio::fs::write(
        pancetta_dir.join("qsos.adi"),
        "rebuild-order test\n\
         <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
         <EOH>\n\
         \n\
         <CALL:5>K2DEF <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
         <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
         <EOR>\n",
    )
    .await
    .unwrap();

    let mut coordinator = live_coordinator_with_home(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();
    assert!(
        wait_for_duplicate(&coordinator.cached_lookup, "K2DEF", FREQ_20M_HZ).await,
        "precondition: startup must rebuild/seed from the ADIF"
    );
    // Let the local writers subscribe after the seed.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");
    super::replay_local_log_tests::complete_one_qso(&manager).await;

    let mut found = Vec::new();
    for _ in 0..100 {
        let db = QsoDatabase::open(&db_path).await.unwrap();
        found = db.get_worked_callsigns("20M").await;
        if found.contains(&"K1DEF".to_string()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        found.contains(&"K1DEF".to_string()),
        "a QSO logged after a startup rebuild must land in the live qso.db: {found:?}"
    );
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// The primary path: a real ADIF in the override dir seeds duplicate
/// detection, and the operator's actual `~/.pancetta` (wherever this
/// test happens to run) is never consulted -- proven by the seed
/// succeeding purely off the isolated fixture.
#[tokio::test]
async fn replay_seeds_duplicate_detection_from_the_override_dir_adif() {
    let tmp = tempfile::tempdir().unwrap();
    let pancetta_dir = tmp.path().join(".pancetta");
    tokio::fs::create_dir_all(&pancetta_dir).await.unwrap();
    tokio::fs::write(
        pancetta_dir.join("qsos.adi"),
        "Pancetta round-4 seed test\n\
         <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
         <EOH>\n\
         \n\
         <CALL:5>K2DEF <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
         <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
         <EOR>\n",
    )
    .await
    .unwrap();

    let mut coordinator = test_coordinator_with_home(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();

    assert!(
        wait_for_duplicate(&coordinator.cached_lookup, "K2DEF", FREQ_20M_HZ).await,
        "a replay run must seed duplicate detection from the (isolated) real ADIF"
    );
}

/// PAN-42: the `--replay` feeder waits on `qso_consumer_ready`, so it
/// must only flip once the history seed is fully applied.
#[tokio::test]
async fn pan42_qso_consumer_ready_fires_only_after_the_replay_history_seed() {
    let tmp = tempfile::tempdir().unwrap();
    let pancetta_dir = tmp.path().join(".pancetta");
    tokio::fs::create_dir_all(&pancetta_dir).await.unwrap();
    let mut adif = String::from("<ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n<EOH>\n");
    for i in 0..5_000u32 {
        let call = format!("W{}X{:04}", i % 10, i);
        adif.push_str(&format!(
            "<CALL:{}>{} <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 \
             <FREQ:9>14.074000 <BAND:3>20m <EOR>\n",
            call.len(),
            call
        ));
    }
    adif.push_str(
        "<CALL:5>K2DEF <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 \
         <FREQ:9>14.074000 <BAND:3>20m <EOR>\n",
    );
    tokio::fs::write(pancetta_dir.join("qsos.adi"), adif)
        .await
        .unwrap();

    let mut coordinator = test_coordinator_with_home(tmp.path()).await;
    let mut ready = coordinator.qso_consumer_ready.subscribe();
    assert!(!*ready.borrow());
    coordinator.start_qso_component().await.unwrap();
    // Current-thread test runtime + no `.await` after the spawn: the task
    // has not been polled yet.
    assert!(
        !*ready.borrow(),
        "start_qso_component returns before the seed runs"
    );
    tokio::time::timeout(std::time::Duration::from_secs(30), ready.wait_for(|r| *r))
        .await
        .expect("QSO consumer never became ready")
        .unwrap();
    assert!(
        coordinator.cached_lookup.is_duplicate("K2DEF", FREQ_20M_HZ),
        "ready must imply the history seed is already applied (no polling)"
    );
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// Rounds 4-6 tried a "snapshot the legacy `qso.db` to a temp file and
/// seed from that" fallback for this exact scenario (no ADIF yet, only a
/// legacy on-disk `qso.db` -- an upgrading operator's migration state).
/// That fallback kept surfacing new correctness/security gaps three
/// rounds running, so it was dropped in favor of graceful degradation
/// (see the block-level comment in `qso.rs` for the full rationale) --
/// this proves the degradation is genuinely graceful: no panic, no
/// error, and the legacy db itself is left completely untouched.
#[tokio::test]
async fn replay_with_no_adif_but_a_legacy_db_starts_cleanly_without_seeding() {
    let tmp = tempfile::tempdir().unwrap();
    let pancetta_dir = tmp.path().join(".pancetta");
    tokio::fs::create_dir_all(&pancetta_dir).await.unwrap();
    let db_path = pancetta_dir.join("qso.db");
    // No qsos.adi in pancetta_dir at all -- the migration state.

    {
        let db = QsoDatabase::open(&db_path).await.unwrap();
        let mut progress = sample_progress("W9LEGACY");
        db.insert_qso(&progress).await.unwrap();
        progress.state = QsoState::Completed {
            their_callsign: "W9LEGACY".to_string(),
            their_report: -9,
            our_report: -12,
            frequency: FREQ_20M_HZ,
            grid_square: None,
            completed_at: chrono::Utc::now(),
            duration_seconds: 90,
        };
        db.update_qso(&progress).await.unwrap();
    }

    let mut coordinator = test_coordinator_with_home(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();

    // Give the (non-)seeding a moment to settle, same window the
    // positive-seeding tests give the spawned task, before asserting
    // the negative.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !coordinator
            .cached_lookup
            .is_duplicate("W9LEGACY", FREQ_20M_HZ),
        "with no ADIF, the legacy db alone must not be seeded from (dropped fallback)"
    );
    // The legacy db itself must be completely untouched.
    let db = QsoDatabase::open(&db_path).await.unwrap();
    let calls = db.get_worked_callsigns("20M").await;
    assert!(
        calls.contains(&"W9LEGACY".to_string()),
        "the real legacy db must be left intact: {:?}",
        calls
    );
}

/// Same rationale as the test above: a corrupt/unreadable `qsos.adi`
/// (even with a legacy `qso.db` present) now degrades gracefully to "no
/// history seeded" rather than falling back to a copy of the legacy db.
#[tokio::test]
async fn replay_with_unreadable_adif_starts_cleanly_without_seeding() {
    let tmp = tempfile::tempdir().unwrap();
    let pancetta_dir = tmp.path().join(".pancetta");
    tokio::fs::create_dir_all(&pancetta_dir).await.unwrap();

    // Non-UTF-8 bytes: `replay_from_adif`'s `read_to_string` fails
    // outright, well before ADIF parsing even starts -- a real, if
    // extreme, case of "adif_path exists but is unreadable."
    tokio::fs::write(pancetta_dir.join("qsos.adi"), [0xFFu8, 0xFE, 0x00, 0x01])
        .await
        .unwrap();

    let db_path = pancetta_dir.join("qso.db");
    {
        let db = QsoDatabase::open(&db_path).await.unwrap();
        let mut progress = sample_progress("W8CORRUPT");
        db.insert_qso(&progress).await.unwrap();
        progress.state = QsoState::Completed {
            their_callsign: "W8CORRUPT".to_string(),
            their_report: -5,
            our_report: -7,
            frequency: FREQ_20M_HZ,
            grid_square: None,
            completed_at: chrono::Utc::now(),
            duration_seconds: 60,
        };
        db.update_qso(&progress).await.unwrap();
    }

    let mut coordinator = test_coordinator_with_home(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !coordinator
            .cached_lookup
            .is_duplicate("W8CORRUPT", FREQ_20M_HZ),
        "an unreadable ADIF must not seed from the legacy db either (dropped fallback)"
    );
}

/// Neither an ADIF nor a legacy db exists (fresh install under
/// `--replay`) -- must not panic or error, just seed nothing.
#[tokio::test]
async fn replay_with_no_history_at_all_starts_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let mut coordinator = test_coordinator_with_home(tmp.path()).await;
    coordinator.start_qso_component().await.unwrap();

    assert!(
        !coordinator
            .cached_lookup
            .is_duplicate("ANYONE", FREQ_20M_HZ),
        "with no history at all, nothing should be seeded as a duplicate"
    );
}
