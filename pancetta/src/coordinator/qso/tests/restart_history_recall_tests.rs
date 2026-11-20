// PAN-260. Formerly the inline module `pan260`.
use super::*;
use pancetta_qso::priority::WorkedStationLookup;

fn manager() -> pancetta_qso::QsoManager {
    let mut m = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        ..Default::default()
    });
    m.require_persistent_history();
    m.set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));
    m
}

#[tokio::test]
async fn failed_live_rebuild_does_not_publish_successful_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let index = dir.path().join("qso.db");
    std::fs::write(&adif, [0xff, 0xfe]).unwrap();
    let m = manager();
    let adif_subscribers = AdifSubscribers::new();
    let (_, _, seed) = start_local_qso_log_writers(
        false,
        &adif,
        &index,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &m,
        &adif_subscribers,
    )
    .await;
    assert!(seed.is_none());
    assert!(pancetta_qso::async_database::QsoDatabase::rebuild_marker(&index).exists());
    assert!(
        prepare_local_qso_index(&adif, &index).await.is_none(),
        "a restart must retry the failed source read"
    );
    assert!(matches!(
        m.respond_to_cq("K1ABC".into(), 1800.0, None).await,
        Err(pancetta_qso::QsoManagerError::Database { .. })
    ));
    assert!(m.get_active_qsos().await.is_empty());
    adif_subscribers
        .stop(std::time::Duration::from_secs(2))
        .await;
}

#[tokio::test]
async fn production_initialization_blocks_restart_recall_and_replay_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let index = dir.path().join("qso.db");
    // Use a fixed date and a deliberately wide window. Boundary semantics are
    // tested with an injected clock in the store and manager tests.
    std::fs::write(&adif, "<EOH>\n<CALL:5>K1ABC <QSO_DATE:8>20260101 <TIME_ON:6>140000 <MODE:3>FT8 <FREQ:9>14.075200 <BAND:3>20M <EOR>\n<CALL:6>JA1XYZ <QSO_DATE:8>20260102 <TIME_ON:6>140000 <MODE:3>FT8 <FREQ:8>7.075200 <BAND:3>40M <EOR>\n").unwrap();
    let adif_subscribers = AdifSubscribers::new();
    let mut first = manager();
    let (_, _, db) = start_local_qso_log_writers(
        false,
        &adif,
        &index,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &first,
        &adif_subscribers,
    )
    .await;
    let db = Arc::new(db.expect("ready history"));
    let lookup = crate::priority_evaluator::CachedStationLookup::new();
    initialize_qso_history(&mut first, &lookup, db.clone())
        .await
        .unwrap();
    assert!(lookup.is_duplicate("K1ABC", 14_074_000.0));
    assert!(lookup.is_duplicate("JA1XYZ", 7_074_000.0));
    assert!(!lookup.is_duplicate("JA1XYZ", 14_074_000.0));
    drop(first);
    let mut config = pancetta_qso::QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        ..Default::default()
    };
    config.duplicate_checking.time_window_hours = 24 * 365 * 100;
    let mut second = pancetta_qso::QsoManager::new(config.clone());
    second.require_persistent_history();
    second.set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));
    let mut events = second.subscribe();
    initialize_qso_history(&mut second, &lookup, db.clone())
        .await
        .unwrap();
    assert!(matches!(
        second.respond_to_cq("K1ABC".into(), 1800.0, None).await,
        Err(pancetta_qso::QsoManagerError::DuplicateQso { .. })
    ));
    assert!(second.get_active_qsos().await.is_empty());
    assert!(events.try_recv().is_err());
    adif_subscribers
        .stop(std::time::Duration::from_secs(2))
        .await;
    db.close().await;
    // A separate real-log fixture makes byte-for-byte replay isolation explicit.
    let replay_dir = tempfile::tempdir().unwrap();
    let source = replay_dir.path().join("qsos.adi");
    let bytes = std::fs::read(&adif).unwrap();
    std::fs::write(&source, &bytes).unwrap();
    let replay = Arc::new(
        pancetta_qso::async_database::QsoDatabase::replay_from_adif(":memory:", &source)
            .await
            .unwrap(),
    );
    let mut replay_manager = pancetta_qso::QsoManager::new(config);
    replay_manager.require_persistent_history();
    replay_manager
        .set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));
    initialize_qso_history(&mut replay_manager, &lookup, replay.clone())
        .await
        .unwrap();
    assert!(matches!(
        replay_manager
            .respond_to_cq("K1ABC".into(), 1800.0, None)
            .await,
        Err(pancetta_qso::QsoManagerError::DuplicateQso { .. })
    ));
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(replay_dir.path()).unwrap().count(), 1);
    replay.close().await;
    let mut failed = manager();
    assert!(initialize_qso_history(&mut failed, &lookup, replay)
        .await
        .is_err());
    assert!(lookup.is_duplicate("JA1XYZ", 7_074_000.0));
    assert!(failed
        .respond_to_cq("K1ABC".into(), 1800.0, None)
        .await
        .is_err());
    assert!(
        pancetta_qso::async_database::QsoDatabase::open(dir.path().join("missing/invalid.db"))
            .await
            .is_err()
    );
    let empty = Arc::new(
        pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap(),
    );
    let mut early_subscription = failed.subscribe();
    initialize_qso_history(&mut failed, &lookup, empty)
        .await
        .unwrap();
    assert!(failed
        .respond_to_cq("K1ABC".into(), 1800.0, None)
        .await
        .is_ok());
    assert!(
        early_subscription.try_recv().is_ok(),
        "history attachment preserves early subscriptions"
    );
}
