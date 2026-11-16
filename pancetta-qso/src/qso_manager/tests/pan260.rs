use super::*;

#[tokio::test]
async fn standalone_memory_only_admission_does_not_require_a_rig_band() {
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        ..Default::default()
    });
    let id = manager
        .respond_to_cq("K1ABC".into(), 1200.0, None)
        .await
        .expect("standalone memory-only admission");
    assert!(
        manager.check_duplicate("K1ABC", 1800.0).await.unwrap(),
        "the active exchange remains reserved at another offset"
    );
    manager.cancel_qso(id).await.unwrap();

    // Persistent-history operation still refuses an unknown TX band.
    manager.attach_database(Arc::new(QsoDatabase::new_in_memory().await.unwrap()));
    assert!(manager
        .respond_to_cq("K1ABC".into(), 1800.0, None)
        .await
        .is_err());
}

#[tokio::test]
async fn completed_history_survives_offsets_bands_restart_and_cleanup() {
    let mut m = QsoManager::new(QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        ..Default::default()
    });
    let dial = Arc::new(AtomicU64::new(14_074_000));
    m.set_dial_frequency_source(dial.clone());
    let now = DateTime::parse_from_rfc3339("2026-11-04T14:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let id = m.start_cq(1200.0, None, false, None).await.unwrap();
    m.qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .start_time = now - Duration::minutes(30);
    for (message, raw) in [
        (
            MessageType::CqResponse {
                calling_station: "W1ABC".into(),
                responding_station: "K1ABC".into(),
                grid: Some("FN31".into()),
            },
            "W1ABC K1ABC FN31",
        ),
        (
            MessageType::ReportAck {
                to_station: "W1ABC".into(),
                from_station: "K1ABC".into(),
                report: -12,
            },
            "W1ABC K1ABC R-12",
        ),
        (
            MessageType::SeventyThree {
                to_station: "W1ABC".into(),
                from_station: "K1ABC".into(),
            },
            "W1ABC K1ABC 73",
        ),
    ] {
        m.process_message(message, raw.into(), 1200.0, Some(-10.0))
            .await
            .unwrap();
    }
    let p = m.get_qso(id).await.unwrap();
    assert!(matches!(p.state, QsoState::Completed { .. }));
    assert_eq!(p.metadata.completed_rf_frequency_hz, Some(14_075_200.0));
    assert!(m.check_duplicate_at("k1abc", 1800.0, now).await.unwrap());

    for (date, expected) in [
        (now - Duration::hours(24), false),
        (now - Duration::hours(24) + Duration::nanoseconds(1), true),
        (now, true),
        (now + Duration::nanoseconds(1), false),
    ] {
        m.qsos
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .metadata
            .start_time = date;
        assert_eq!(
            m.check_duplicate_at("K1ABC", 1800.0, now).await.unwrap(),
            expected
        );
    }
    m.qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .start_time = p.metadata.start_time;

    dial.store(7_074_000, Ordering::Relaxed);
    assert!(!m.check_duplicate_at("K1ABC", 1800.0, now).await.unwrap());
    m.config.duplicate_checking.check_frequency = false;
    assert!(m.check_duplicate_at("K1ABC", 1800.0, now).await.unwrap());
    m.config.duplicate_checking.check_frequency = true;
    let split = Arc::new(AtomicU64::new(14_074_000));
    m.set_split_tx_frequency_source(split.clone());
    assert!(m.check_duplicate_at("K1ABC", 1800.0, now).await.unwrap());
    split.store(0, Ordering::Relaxed);
    dial.store(14_074_000, Ordering::Relaxed);
    let db = Arc::new(QsoDatabase::new_in_memory().await.unwrap());
    let mut logged = p.clone();
    logged.metadata.frequency = p.metadata.completed_rf_frequency_hz.unwrap();
    db.insert_qso(&logged).await.unwrap();
    m.attach_database(db.clone());
    // Force the normal cleanup age without changing the fixed start-time window.
    if let QsoState::Completed { completed_at, .. } =
        &mut m.qsos.write().await.get_mut(&id).unwrap().state
    {
        *completed_at = Utc::now() - Duration::hours(2);
    }
    m.cleanup_completed_qsos().await;
    assert!(m.qsos.read().await.is_empty());
    assert!(m.check_duplicate_at("K1ABC", 1800.0, now).await.unwrap());
    let mut fresh = QsoManager::with_database(m.config.clone(), db.clone());
    fresh.set_dial_frequency_source(dial.clone());
    fresh.require_persistent_history();
    assert!(fresh
        .clone()
        .check_duplicate_at("K1ABC", 1800.0, now)
        .await
        .unwrap());
    assert!(!fresh
        .check_duplicate_at("K1ABC", 1800.0, now + Duration::hours(24))
        .await
        .unwrap());
    dial.store(0, Ordering::Relaxed);
    assert!(fresh
        .check_duplicate_at("K1ABC", 1800.0, now)
        .await
        .is_err());
    fresh.config.duplicate_checking.time_window_hours = 0;
    assert!(!fresh
        .check_duplicate_at("K1ABC", 1800.0, now)
        .await
        .unwrap());
    fresh.config.duplicate_checking.time_window_hours = 24;
    fresh.config.duplicate_checking.enabled = false;
    assert!(!fresh
        .check_duplicate_at("K1ABC", 1800.0, now)
        .await
        .unwrap());
    fresh.config.duplicate_checking.enabled = true;
    dial.store(14_074_000, Ordering::Relaxed);
    db.close().await;
    let mut events = fresh.subscribe();
    assert!(fresh
        .respond_to_cq("K1ABC".into(), 1800.0, None)
        .await
        .is_err());
    assert!(fresh.get_active_qsos().await.is_empty());
    assert!(events.try_recv().is_err());
    assert!(fresh
        .respond_to_cq_manual("K1ABC".into(), 1800.0, None)
        .await
        .is_ok());
    let mut missing = QsoManager::new(m.config.clone());
    missing.require_persistent_history();
    missing.set_dial_frequency_source(dial);
    missing.qsos.write().await.insert(id, p);
    assert!(missing
        .check_duplicate_at("K1ABC", 1800.0, now)
        .await
        .is_err());
}
