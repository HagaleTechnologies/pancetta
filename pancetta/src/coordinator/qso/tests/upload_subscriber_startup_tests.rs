//! The QSO upload subscriber and the ADIF writer hear a QSO that completes
//! while the Qso task is still starting up, before its startup index rebuild
//! and logger open.

use crate::coordinator::ApplicationCoordinator;
use pancetta_config::Config;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// A QRZ XML endpoint that accepts every connection and never answers. The
/// receiver gets one `()` per accepted connection.
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

/// A live coordinator rooted at `home`, station W1ABC, with only QRZ XML
/// enrichment on. That starts the upload subscriber without any upload
/// destination, so nothing leaves the machine except the lookup to
/// `qrz_url`.
async fn qrz_only_coordinator(home: &std::path::Path, qrz_url: String) -> ApplicationCoordinator {
    let mut config = Config::default();
    config.station.callsign = "W1ABC".to_string();
    config.network.qrz_xml.enabled = true;
    config.network.qrz_xml.username = "user".to_string();
    config.network.qrz_xml.password = "pass".to_string();
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true, // no_audio
        true, // headless
        None, // no WAV
        None, // live, not --replay
        None, // no test-tx
        1500.0,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
        std::env::temp_dir().join("pancetta-test-config.toml"),
        true,
        None, // no session-home override
    )
    .await
    .expect("coordinator creation should succeed");
    coordinator.pancetta_home_override = Some(home.to_path_buf());
    coordinator.qrz_xml_base_url_override = Some(qrz_url);
    coordinator
}

/// The manager is published before the Qso task runs, and that task starts
/// the ADIF writer and upload subscriber only during its startup index work.
/// A QSO completed through the published manager before the task has run at
/// all (the test runtime is current-thread, so nothing spawned has been
/// polled yet) still reaches the upload subscriber, which looks the station
/// up on QRZ, and the logbook of record.
#[tokio::test]
async fn a_qso_completed_before_the_qso_task_runs_reaches_uploads_and_the_logbook() {
    let tmp = tempfile::tempdir().unwrap();
    let (qrz_url, mut qrz_connections) = silent_qrz_server();
    let mut coordinator = qrz_only_coordinator(tmp.path(), qrz_url).await;
    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published");

    super::replay_local_log_tests::complete_one_qso(&manager).await;

    tokio::time::timeout(Duration::from_secs(10), qrz_connections.recv())
        .await
        .expect("the upload subscriber never saw the K1DEF completion")
        .expect("server thread alive");
    let adif_path = tmp.path().join(".pancetta").join("qsos.adi");
    assert_eq!(
        super::replay_local_log_tests::wait_for_adif_record(&adif_path).await,
        1,
        "the K1DEF completion is not in qsos.adi"
    );
    coordinator.shutdown().await.unwrap();
}
