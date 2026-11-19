// PAN-382. `start_qso_component` broadcasts one N1MM `<contactinfo>` datagram
// per completed QSO on a live run, none under `--replay`, and keeps
// broadcasting after the shutdown flag flips (PAN-232/PAN-274 lifetime rule).
use super::super::ApplicationCoordinator;
use super::replay_local_log_tests::{complete_one_qso, wait_for_adif_record};
use pancetta_config::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

/// A coordinator rooted at `home`, station call W1ABC (the call
/// `complete_one_qso` works as), with `[logging] n1mm_udp` set to `n1mm`.
async fn coordinator_with_home(
    home: &std::path::Path,
    replay: bool,
    n1mm: Option<String>,
) -> ApplicationCoordinator {
    let mut config = Config::default();
    config.station.callsign = "W1ABC".to_string();
    config.logging.n1mm_udp = n1mm;
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true, // no_audio
        true, // headless
        None, // no WAV
        replay.then(|| std::path::PathBuf::from("/some/capture/dir")),
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

async fn listener() -> (UdpSocket, String) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind listener");
    let addr = socket.local_addr().expect("listener addr").to_string();
    (socket, addr)
}

async fn recv_within(socket: &UdpSocket, wait: Duration) -> Option<String> {
    let mut buf = vec![0u8; 65_536];
    match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(String::from_utf8(buf[..n].to_vec()).expect("utf-8 datagram")),
        Ok(Err(e)) => panic!("recv_from failed: {e}"),
        Err(_) => None,
    }
}

/// The text of `<name>…</name>` in a datagram (values here need no unescaping).
fn element<'a>(xml: &'a str, name: &str) -> &'a str {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml
        .find(&open)
        .unwrap_or_else(|| panic!("{name} missing in {xml}"))
        + open.len();
    let end = start
        + xml[start..]
            .find(&close)
            .unwrap_or_else(|| panic!("{name} not closed in {xml}"));
    &xml[start..end]
}

fn manager_of(coordinator: &ApplicationCoordinator) -> pancetta_qso::QsoManager {
    coordinator
        .qso_manager_watch
        .borrow()
        .clone()
        .expect("QSO manager published")
}

async fn wait_for_qso_consumer(coordinator: &ApplicationCoordinator) {
    let mut ready = coordinator.qso_consumer_ready.subscribe();
    tokio::time::timeout(Duration::from_secs(30), ready.wait_for(|ready| *ready))
        .await
        .expect("QSO consumer startup timed out")
        .expect("QSO consumer readiness channel closed");
}

#[tokio::test]
async fn completion_before_qso_consumer_is_ready_is_still_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let (socket, addr) = listener().await;
    let mut coordinator = coordinator_with_home(tmp.path(), false, Some(addr)).await;
    coordinator.start_qso_component().await.unwrap();

    let manager = manager_of(&coordinator);
    complete_one_qso(&manager).await;
    assert!(!*coordinator.qso_consumer_ready.borrow());
    wait_for_qso_consumer(&coordinator).await;

    let xml = recv_within(&socket, Duration::from_secs(3))
        .await
        .expect("contactinfo for the QSO completed during startup");
    assert_eq!(element(&xml, "call"), "K1DEF");
    coordinator.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_qso_component_broadcasts_one_contactinfo_per_completed_qso() {
    let tmp = tempfile::tempdir().unwrap();
    let (socket, addr) = listener().await;
    let mut coordinator = coordinator_with_home(tmp.path(), false, Some(addr)).await;
    coordinator.start_qso_component().await.unwrap();
    wait_for_qso_consumer(&coordinator).await;
    let manager = manager_of(&coordinator);
    complete_one_qso(&manager).await;

    let xml = recv_within(&socket, Duration::from_secs(3))
        .await
        .expect("one contactinfo datagram for the completed QSO");
    assert!(xml.contains("<contactinfo>"), "{xml}");
    assert_eq!(element(&xml, "call"), "K1DEF");
    assert_eq!(element(&xml, "band"), "14");
    assert_eq!(element(&xml, "mode"), "FT8");
    assert_eq!(element(&xml, "mycall"), "W1ABC");
    assert_eq!(element(&xml, "snt"), "-11");
    assert_eq!(element(&xml, "rcv"), "-12");
    assert_eq!(element(&xml, "gridsquare"), "FN31");

    // The datagram's time is the `qsos.adi` record's time.
    let adif_path = tmp.path().join(".pancetta").join("qsos.adi");
    assert_eq!(wait_for_adif_record(&adif_path).await, 1);
    let adif = std::fs::read_to_string(&adif_path)
        .unwrap()
        .to_ascii_lowercase();
    let timestamp = element(&xml, "timestamp");
    let (date, time) = timestamp.split_once(' ').expect("date and time");
    let date = date.replace('-', "");
    let time = time.replace(':', "");
    assert!(
        adif.contains(&format!("<qso_date:8>{date}")),
        "QSO_DATE {date} not in {adif}"
    );
    assert!(
        adif.contains(&format!("<time_on:6>{time}")),
        "TIME_ON {time} not in {adif}"
    );

    assert!(
        recv_within(&socket, Duration::from_millis(500))
            .await
            .is_none(),
        "a second datagram arrived for one QSO"
    );
    coordinator.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_qso_component_sends_no_contactinfo() {
    let tmp = tempfile::tempdir().unwrap();
    let (socket, addr) = listener().await;
    let mut coordinator = coordinator_with_home(tmp.path(), true, Some(addr)).await;
    coordinator.start_qso_component().await.unwrap();
    wait_for_qso_consumer(&coordinator).await;
    let manager = manager_of(&coordinator);
    complete_one_qso(&manager).await;
    assert!(
        recv_within(&socket, Duration::from_secs(1)).await.is_none(),
        "a replayed QSO was broadcast"
    );
    coordinator.shutdown_signal.store(true, Ordering::Release);
}

#[tokio::test]
async fn qso_completed_after_the_shutdown_flag_is_still_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let (socket, addr) = listener().await;
    let mut coordinator = coordinator_with_home(tmp.path(), false, Some(addr)).await;
    coordinator.start_qso_component().await.unwrap();
    wait_for_qso_consumer(&coordinator).await;
    let manager = manager_of(&coordinator);
    coordinator.shutdown_signal.store(true, Ordering::Release);
    tokio::time::sleep(Duration::from_millis(200)).await;
    complete_one_qso(&manager).await;
    let xml = recv_within(&socket, Duration::from_secs(3))
        .await
        .expect("a QSO completed after the shutdown flag is still broadcast");
    assert_eq!(element(&xml, "call"), "K1DEF");
    coordinator.shutdown().await.unwrap();
}
