//! PAN-359: cqdx.io's activation feed fills the worked-reference book
//! (AC11, AC12, AC13).

use super::tests::{cfg, mount, TOKEN};
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use pancetta_qso::{
    AdifLogWriter, AdifProcessor, CompletionAired, QsoEvent, QsoManager, QsoManagerConfig,
    UnsentAttemptPayload,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn row(call: &str, program: &str, reference: &str, spot_time: Option<&str>) -> Activation {
    Activation {
        callsign: call.to_string(),
        program: program.to_string(),
        reference: reference.to_string(),
        spot_time: spot_time.map(str::to_string),
    }
}

fn worked(sig: &str, sig_info: &str) -> WorkedReference {
    WorkedReference {
        sig: sig.to_string(),
        sig_info: sig_info.to_string(),
    }
}

/// Mount the startup endpoints so the needed list loads and spots poll.
async fn mount_startup(server: &MockServer) {
    mount(
        server,
        "/api/v1/entities",
        200,
        serde_json::json!({"entities": []}),
    )
    .await;
    mount(
        server,
        "/api/v1/entities/needed",
        200,
        serde_json::json!({"needed": []}),
    )
    .await;
    mount(
        server,
        "/api/v1/entities/needed-grids",
        200,
        serde_json::json!({"grids": []}),
    )
    .await;
}

/// PAN-359 AC11: type maps to SIG (case-insensitive), other programmes and
/// invalid rows are skipped, references are upper-cased, and the newest
/// `spotTime` wins per callsign (unparseable oldest, ties keep the first).
#[test]
fn activation_rows_map_to_worked_references() {
    let rows = vec![
        row("dl1abc", "POTA", " da-0123 ", Some("2026-11-22T09:01:00Z")),
        row("W2XYZ/P", "sota", "W2/WE-003", Some("2026-11-22T09:00:00Z")),
        // Other programmes are skipped.
        row("G4AAA", "wwff", "GFF-0001", Some("2026-11-22T09:00:00Z")),
        // Invalid callsign or reference rows are skipped.
        row("K1<X>", "pota", "K-0001", None),
        row("K1ZZZ", "pota", "K", None),
        row("K1YYY", "pota", "K-00<1>", None),
        row("", "pota", "K-0002", None),
        // Newer spot for DL1ABC wins.
        row("DL1ABC", "pota", "DA-0456", Some("2026-11-22T09:05:00Z")),
        // Older spot for DL1ABC loses.
        row("DL1ABC", "pota", "DA-0999", Some("2026-11-22T08:00:00Z")),
        // Unparseable time ranks oldest: the first parseable row stays.
        row("W2XYZ/P", "SOTA", "W2/WE-999", Some("yesterday")),
        // Tie: both without a time; the first row is kept.
        row("JA1AAA", "pota", "JA-0001", None),
        row("JA1AAA", "pota", "JA-0002", None),
        // A parseable time beats a missing one.
        row("VK2BBB", "sota", "VK2/HU-001", None),
        row(
            "VK2BBB",
            "sota",
            "VK2/HU-002",
            Some("2026-11-22T07:00:00+00:00"),
        ),
    ];
    assert_eq!(
        worked_references_from_activations(&rows),
        vec![
            ("DL1ABC".to_string(), worked("POTA", "DA-0456")),
            ("W2XYZ/P".to_string(), worked("SOTA", "W2/WE-003")),
            ("JA1AAA".to_string(), worked("POTA", "JA-0001")),
            ("VK2BBB".to_string(), worked("SOTA", "VK2/HU-002")),
        ]
    );
    assert!(worked_references_from_activations(&[]).is_empty());
}

/// PAN-359 AC12 (scenario 2 end to end): the spot poller fills the shared
/// book from `/api/v1/pota`, and a QSO with DL1ABC completed by a
/// `QsoManager` sharing that book logs SIG=POTA and SIG_INFO=DA-0123.
#[tokio::test]
async fn cqdx_activation_reaches_logged_record() {
    let server = MockServer::start().await;
    mount_startup(&server).await;
    mount(
        &server,
        "/api/v1/spots",
        200,
        serde_json::json!({"groups": []}),
    )
    .await;
    let spot_time = chrono::Utc::now().to_rfc3339();
    mount(
        &server,
        "/api/v1/pota",
        200,
        serde_json::json!({
            "activations": [
                {"callsign": "DL1ABC", "type": "pota", "reference": "DA-0123",
                 "name": "Some Park", "frequency": "14074", "mode": "FT8",
                 "spotTime": spot_time},
                {"callsign": "7N1ICA/1", "type": "sota", "reference": "JA/YN-036",
                 "name": "Onigatake", "frequency": 7.005, "mode": "CW",
                 "spotTime": spot_time}
            ],
            "byCallsign": {"DL1ABC": []},
            "updatedAt": 1_795_424_460
        }),
    )
    .await;

    let book = WorkedReferenceBook::new();
    let lookup = Arc::new(CachedStationLookup::new());
    let config = pancetta_config::network::CqdxConfig {
        poll_interval_secs: 10,
        ..cfg(&server.uri(), Some(TOKEN))
    };
    let bridge = Arc::new(
        CqdxBridge::from_config(&config, lookup)
            .unwrap()
            .with_worked_references(book.clone()),
    );
    bridge.startup().await.unwrap();
    let handle = bridge.spawn_spot_poller(
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
        None,
        None,
    );
    let mut filled = false;
    for _ in 0..100 {
        if book.lookup("DL1ABC", chrono::Utc::now()).is_some() {
            filled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.abort();
    assert!(filled, "the poller never filled the book");

    // A QsoManager sharing the book completes a QSO with DL1ABC (our 73
    // queued, then aired).
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
        our_grid: Some("EM12".into()),
        ..QsoManagerConfig::default()
    });
    manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
    manager.set_worked_reference_source(book.clone());
    let reply = |step| {
        let manager = &manager;
        async move {
            manager
                .respond_to_caller_with_outcome(
                    "DL1ABC".into(),
                    1500.0,
                    Some(SlotParity::Even),
                    step,
                    Some(-8.0),
                    Some(-4),
                    None,
                    false,
                    None,
                )
                .await
                .expect("reply")
        }
    };
    let (id, _) = reply(ResponseStep::ReportAck).await;
    let mut rx = manager.subscribe();
    let (_, outcome) = reply(ResponseStep::SeventyThree).await;
    let aired = manager
        .confirm_completion_aired(
            id,
            outcome.attempt_id(),
            "",
            &UnsentAttemptPayload {
                frequency_hz: 1500.0,
                remote_binding: None,
            },
            chrono::Utc::now(),
        )
        .await;
    assert_eq!(aired, CompletionAired::Committed);
    let mut metadata = None;
    while let Ok(event) = rx.try_recv() {
        if let QsoEvent::QsoCompleted { metadata: m, .. } = event {
            metadata = Some(m);
        }
    }
    let metadata = metadata.expect("QsoCompleted emitted");

    let dir = tempfile::tempdir().unwrap();
    let adi = dir.path().join("qsos.adi");
    let writer = AdifLogWriter::open(&adi).await.unwrap();
    writer.append(&metadata).await.unwrap();
    let text = std::fs::read_to_string(&adi).unwrap();
    let file = AdifProcessor::new().parse_string(&text).unwrap();
    assert_eq!(file.records.len(), 1, "{text}");
    let fields: HashMap<String, String> = file.records[0]
        .fields
        .iter()
        .map(|(k, f)| (k.to_ascii_uppercase(), f.value.clone()))
        .collect();
    assert_eq!(fields.get("CALL").map(String::as_str), Some("DL1ABC"));
    assert_eq!(
        fields.get("SIG").map(String::as_str),
        Some("POTA"),
        "{text}"
    );
    assert_eq!(
        fields.get("SIG_INFO").map(String::as_str),
        Some("DA-0123"),
        "{text}"
    );
}

/// PAN-359 AC13: an activation feed answering 404 or 500 never stops or
/// counts against live spot polling: `/api/v1/spots` is still requested on
/// every tick.
#[tokio::test]
async fn activation_feed_failure_does_not_stop_spot_polling() {
    for status in [404, 500] {
        let server = MockServer::start().await;
        mount_startup(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/pota"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"groups": []})),
            )
            .expect(3..)
            .mount(&server)
            .await;

        let book = WorkedReferenceBook::new();
        let lookup = Arc::new(CachedStationLookup::new());
        let config = pancetta_config::network::CqdxConfig {
            poll_interval_secs: 1,
            ..cfg(&server.uri(), Some(TOKEN))
        };
        let bridge = Arc::new(
            CqdxBridge::from_config(&config, lookup)
                .unwrap()
                .with_worked_references(book.clone()),
        );
        bridge.startup().await.unwrap();
        let handle = bridge.spawn_spot_poller(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            None,
            None,
        );
        // Ticks at 0 s, 1 s and 2 s.
        tokio::time::sleep(Duration::from_millis(2_600)).await;
        handle.abort();
        server.verify().await;
        assert!(
            book.lookup("DL1ABC", chrono::Utc::now()).is_none(),
            "status {status} wrote nothing"
        );
    }
}
