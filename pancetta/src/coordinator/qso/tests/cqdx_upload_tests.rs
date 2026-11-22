use super::{cqdx_logbook_upload_enabled, cqdx_record_from_metadata, logbook_upload_enabled};
use pancetta_config::network::CqdxConfig;
use pancetta_core::time::UtcTimestamp;
use pancetta_qso::{GridSquares, QsoMetadata, SignalReports};

/// Default cqdx config (disabled, no token) must NOT enable the upload —
/// the subscriber stays dormant unless the operator opts in.
#[test]
fn upload_disabled_by_default() {
    let cfg = CqdxConfig::default();
    assert!(!cfg.enabled);
    assert!(!cqdx_logbook_upload_enabled(&cfg));
}

/// Enabled but with no token (or an empty token) must NOT enable the upload
/// — we never POST without auth.
#[test]
fn upload_requires_token() {
    let mut cfg = CqdxConfig {
        enabled: true,
        token: None,
        ..Default::default()
    };
    assert!(!cqdx_logbook_upload_enabled(&cfg));

    cfg.token = Some(String::new());
    assert!(!cqdx_logbook_upload_enabled(&cfg));
}

/// Enabled + a non-empty token opts in.
#[test]
fn upload_enabled_with_token() {
    let cfg = CqdxConfig {
        enabled: true,
        token: Some("pat_abc123def456".to_string()),
        ..Default::default()
    };
    assert!(cqdx_logbook_upload_enabled(&cfg));
}

/// A token without `enabled` is still off (belt-and-suspenders).
#[test]
fn upload_off_when_disabled_even_with_token() {
    let cfg = CqdxConfig {
        enabled: false,
        token: Some("pat_abc123def456".to_string()),
        ..Default::default()
    };
    assert!(!cqdx_logbook_upload_enabled(&cfg));
}

fn metadata_with_call(call: Option<&str>) -> QsoMetadata {
    let now = UtcTimestamp::now();
    QsoMetadata {
        qso_id: pancetta_qso::QsoId::new_v4(),
        our_callsign: "K5ARH".to_string(),
        their_callsign: call.map(str::to_string),
        frequency: 14_074_000.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".to_string(),
        first_answer_at: None,
        start_time: now,
        end_time: Some(now + jiff::SignedDuration::from_secs(90)),
        reports: SignalReports {
            sent: Some(-8),
            received: Some(-12),
        },
        grids: GridSquares {
            ours: Some("EM10".to_string()),
            theirs: Some("PM95".to_string()),
        },
        their_state: None,
        contest_info: None,
        tags: std::collections::HashMap::new(),
        adif_field_types: std::collections::HashMap::new(),
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
    }
}

#[test]
fn pan300_lotw_target_and_replay_matrix() {
    use super::super::upload_outbox::outbox_destinations;
    use pancetta_tui::upload_status::UploadTarget;
    // LoTW alone, mixed, disabled, all off, and QRZ XML alone.
    for (http, lotw, xml) in [
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (false, false, false),
        (false, false, true),
    ] {
        for replay in [false, true] {
            let destinations = outbox_destinations(http, http, http, http, replay, lotw);
            let targets = super::completion_upload_targets(&destinations);
            assert_eq!(
                targets
                    .iter()
                    .filter(|(t, _)| *t == UploadTarget::Lotw)
                    .count(),
                usize::from(lotw && !replay)
            );
            if replay {
                assert!(destinations.is_empty(), "no worker or outbox opened");
                assert!(!super::logbook_upload_enabled(
                    http, http, lotw, http, http, xml, true
                ));
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pan300_lotw_only_completion_is_queued_once() {
    use super::super::upload_outbox::{self, LogbookClients, OutboxTiming, UploadHealth};
    use pancetta_dx::upload_outbox::{OutboxStore, RetrySchedule, UploadDestination};
    use pancetta_tui::upload_status::{UploadState, UploadTarget};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("fake tqsl");
    let invoked = dir.path().join("invoked");
    let release = dir.path().join("release");
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\necho attempt >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nexit 0\n",
            invoked.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let clients = LogbookClients {
        lotw: Some(Arc::new(pancetta_dx::LotwUploadClient::new(
            binary.to_str().unwrap(),
            "Home Station",
        ))),
        clublog: None,
        qrz: None,
        eqsl: None,
        cqdx: None,
    };
    let statuses = super::super::qso_upload_status::UploadStatusRegistry::shared();
    let shutdown = Arc::new(AtomicBool::new(false));
    let path = dir.path().join("queue.json");
    let (handle, task) = upload_outbox::start_upload_outbox(
        path.clone(),
        Arc::new(clients),
        RetrySchedule::new(3, 10, 40, 2.0, 0.0, Duration::from_millis(100)),
        OutboxTiming::PRODUCTION,
        Arc::new(Mutex::new(UploadHealth::default())),
        None,
        shutdown.clone(),
        Some(statuses.clone()),
    );
    let (tx, rx) = tokio::sync::broadcast::channel(4);
    super::start_qso_upload_subscriber(
        Some(handle.clone()),
        Default::default(),
        100,
        false,
        rx,
        shutdown.clone(),
        Arc::new(crate::priority_evaluator::CachedStationLookup::new()),
        None,
        statuses.clone(),
    );
    let metadata = metadata_with_call(Some("K1ABC"));
    let uuid = metadata.qso_id;
    tx.send(pancetta_qso::QsoEvent::QsoCompleted {
        qso_id: uuid,
        metadata,
        state_history: vec![],
        messages: vec![],
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !invoked.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (store, quarantine) = OutboxStore::open(path.clone());
    assert!(quarantine.is_none());
    assert_eq!(store.entries().len(), 1);
    assert_eq!(store.entries()[0].qso_id, uuid);
    assert_eq!(store.entries()[0].destination, UploadDestination::Lotw);
    assert!(store.entries()[0].payload.contains("K1ABC"));
    std::fs::write(release, "").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let accepted = statuses.lock().unwrap().snapshot().iter().any(|s| {
                s.uuid == uuid.to_string()
                    && s.state(UploadTarget::Lotw) == Some(UploadState::Accepted)
            });
            if accepted && OutboxStore::open(path.clone()).0.entries().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(std::fs::read_to_string(invoked).unwrap().lines().count(), 1);
    shutdown.store(true, Ordering::Release);
    drop(tx);
    drop(handle);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn pan298_upload_payloads_follow_attribution_setting() {
    use super::super::upload_outbox::{
        self, Delivery, OutboxTiming, OutboxTransport, UploadHealth,
    };
    use pancetta_dx::upload_outbox::{RetrySchedule, UploadDestination};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    struct Transport(Arc<Mutex<Vec<(UploadDestination, String)>>>);
    impl OutboxTransport for Transport {
        fn destinations(&self) -> Vec<UploadDestination> {
            vec![UploadDestination::QrzLogbook, UploadDestination::Cqdx]
        }
        async fn deliver(&self, destination: UploadDestination, payload: String) -> Delivery {
            self.0.lock().unwrap().push((destination, payload));
            Delivery::Accepted {
                duplicate: false,
                remote_id: None,
            }
        }
    }
    for enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let statuses = super::super::qso_upload_status::UploadStatusRegistry::shared();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, task) = upload_outbox::start_upload_outbox(
            dir.path().join("queue.json"),
            Arc::new(Transport(payloads.clone())),
            RetrySchedule::new(3, 5, 20, 2.0, 0.0, std::time::Duration::from_millis(100)),
            OutboxTiming::PRODUCTION,
            Arc::new(Mutex::new(UploadHealth::default())),
            None,
            shutdown.clone(),
            Some(statuses.clone()),
        );
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        super::start_qso_upload_subscriber(
            Some(handle.clone()),
            Default::default(),
            100,
            enabled,
            rx,
            shutdown.clone(),
            Arc::new(crate::priority_evaluator::CachedStationLookup::new()),
            None,
            statuses,
        );
        let metadata = metadata_with_call(Some("K1ABC"));
        tx.send(pancetta_qso::QsoEvent::QsoCompleted {
            qso_id: metadata.qso_id,
            metadata,
            state_history: vec![],
            messages: vec![],
        })
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while payloads.lock().unwrap().len() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        for (destination, payload) in payloads.lock().unwrap().iter() {
            if *destination == UploadDestination::QrzLogbook {
                let p = pancetta_qso::AdifProcessor::new();
                let parsed = p.parse_string(payload).unwrap();
                let qso = p.record_to_qso(&parsed.records[0]).unwrap();
                assert_eq!(qso.comment.as_deref(), enabled.then_some("Using cqdx.io & Pancetta -- https://cqdx.io https://github.com/HagaleTechnologies/pancetta"));
            } else {
                let json: serde_json::Value = serde_json::from_str(payload).unwrap();
                assert!(json.get("comment").is_none());
                assert!(!payload.contains("Using cqdx.io & Pancetta"));
            }
        }
        shutdown.store(true, Ordering::Release);
        drop(tx);
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn pan344_subscriber_missing_payload_and_closed_worker() {
    use super::super::upload_outbox::{
        self, Delivery, OutboxTiming, OutboxTransport, UploadHealth,
    };
    use pancetta_dx::upload_outbox::{RetrySchedule, UploadDestination};
    use pancetta_tui::upload_status::{UploadState, UploadTarget};
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    struct Transport;
    impl OutboxTransport for Transport {
        fn destinations(&self) -> Vec<UploadDestination> {
            vec![UploadDestination::QrzLogbook, UploadDestination::Cqdx]
        }
        async fn deliver(&self, _: UploadDestination, _: String) -> Delivery {
            Delivery::Accepted {
                duplicate: false,
                remote_id: None,
            }
        }
    }
    for closed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let statuses = super::super::qso_upload_status::UploadStatusRegistry::shared();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, task) = upload_outbox::start_upload_outbox(
            dir.path().join("queue.json"),
            Arc::new(Transport),
            RetrySchedule::new(3, 5, 20, 2.0, 0.0, std::time::Duration::from_millis(100)),
            OutboxTiming::PRODUCTION,
            Arc::new(Mutex::new(UploadHealth::default())),
            None,
            shutdown.clone(),
            Some(statuses.clone()),
        );
        if closed {
            task.abort();
            while !handle.worker_stopped() {
                tokio::task::yield_now().await;
            }
        }
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        super::start_qso_upload_subscriber(
            Some(handle),
            Default::default(),
            100,
            false,
            rx,
            shutdown,
            Arc::new(crate::priority_evaluator::CachedStationLookup::new()),
            None,
            statuses.clone(),
        );
        let metadata = metadata_with_call(if closed { Some("K1ABC") } else { None });
        let uuid = metadata.qso_id;
        tx.send(pancetta_qso::QsoEvent::QsoCompleted {
            qso_id: uuid,
            metadata,
            state_history: vec![],
            messages: vec![],
        })
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let ready = statuses
                    .lock()
                    .unwrap()
                    .snapshot()
                    .first()
                    .is_some_and(|s| {
                        s.state(UploadTarget::Cqdx) == Some(UploadState::Failed)
                            && s.state(UploadTarget::Qrz)
                                == Some(if closed {
                                    UploadState::Failed
                                } else {
                                    UploadState::Accepted
                                })
                    });
                if ready {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("subscriber reports actual payload and queue outcomes");
        drop(tx);
        task.abort();
    }
}

/// The structured cqdx record carries the dial+offset RF frequency,
/// both grids, and stringified SNR reports the API expects.
#[test]
fn record_maps_metadata_fields() {
    let mut md = metadata_with_call(Some("JA1ABC"));
    let rec = cqdx_record_from_metadata(&md).expect("record");
    assert_eq!(rec.callsign, "JA1ABC");
    assert_eq!(rec.frequency, 14_074_000);
    assert_eq!(rec.mode, "FT8");
    assert_eq!(rec.remote_grid.as_deref(), Some("PM95"));
    assert_eq!(rec.local_grid.as_deref(), Some("EM10"));
    assert_eq!(rec.rst_sent.as_deref(), Some("-8"));
    assert_eq!(rec.rst_received.as_deref(), Some("-12"));
    assert_eq!(rec.start_time, md.start_time);
    assert_eq!(rec.end_time, md.end_time.unwrap());
    md.first_answer_at = Some(md.start_time + jiff::SignedDuration::from_mins(4));
    let rec = cqdx_record_from_metadata(&md).expect("answered record");
    assert_eq!(rec.start_time, md.first_answer_at.unwrap());
}

/// No contra-callsign → nothing to upload.
#[test]
fn record_none_without_callsign() {
    let md = metadata_with_call(None);
    assert!(cqdx_record_from_metadata(&md).is_none());
}

/// `--replay` forces the logbook-upload subscriber off even when every
/// target is enabled — a replayed "completed" QSO must never be filed as
/// a real contact.
#[test]
fn replay_disables_upload_even_with_every_target_enabled() {
    assert!(!logbook_upload_enabled(
        true, true, true, true, true, true, true
    ));
}

/// With replay off, at least one enabled target is sufficient.
#[test]
fn any_single_target_enables_upload_when_not_replaying() {
    assert!(logbook_upload_enabled(
        true, false, false, false, false, false, false
    ));
    assert!(logbook_upload_enabled(
        false, true, false, false, false, false, false
    ));
    assert!(logbook_upload_enabled(
        false, false, true, false, false, false, false
    ));
    assert!(logbook_upload_enabled(
        false, false, false, true, false, false, false
    ));
    assert!(logbook_upload_enabled(
        false, false, false, false, true, false, false
    ));
    assert!(logbook_upload_enabled(
        false, false, false, false, false, true, false
    ));
}

/// No target enabled → no upload, replay or not.
#[test]
fn no_targets_enabled_means_no_upload() {
    assert!(!logbook_upload_enabled(
        false, false, false, false, false, false, false
    ));
    assert!(!logbook_upload_enabled(
        false, false, false, false, false, false, true
    ));
}
