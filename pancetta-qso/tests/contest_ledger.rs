//! Durable PAN-50 contest evidence, including ADIF-only cold recovery.
use pancetta_qso::adif::AdifProcessor;

fn app_record(version: u32) -> String {
    let context = serde_json::json!({
        "version": version, "profile_id":"us-state-qso-party",
        "operator_callsign":"K5ARH", "operator_region":"TX",
        "periods":[{"start":"2026-08-29T14:00:00Z", "end":"2026-08-30T02:00:00Z"}],
        "evidence":{"their_region":null,"their_county":null,"our_county":null,"source":"import"}
    })
    .to_string();
    format!("<CALL:4>K0KS<QSO_DATE:8>20260829<TIME_ON:6>143000<FREQ:6>14.074<MODE:3>FT8<STATE:2>KS<CNTY:5>KS,SB<APP_PANCETTA_CONTEST_INSTANCE:4>test<APP_PANCETTA_CONTEST_RULES:12>ksqp-ft-2026<APP_PANCETTA_CONTEST_CONTACT_ID:36>9b16f2be-7d63-4a89-b789-f863df27bccf<APP_PANCETTA_CONTEST_CONTEXT:{}>{context}<EOR>", context.len())
    // The rules id has twelve bytes including the hyphens.
}

#[test]
fn family_contest_recovers_without_standard_contest_id() {
    let p = AdifProcessor::new();
    let file = p.parse_string(&app_record(1)).unwrap();
    let adif = p.record_to_qso(&file.records[0]).unwrap();
    let metadata = p.adif_to_qso(&adif);
    assert!(metadata.contest_info.as_ref().unwrap().scoring.is_some());
    let exported = p.qso_to_adif(&metadata, metadata.contest_info.as_ref());
    assert!(exported.contest_id.is_none());
    let json: serde_json::Value =
        serde_json::from_str(&exported.additional_fields["APP_PANCETTA_CONTEST_CONTEXT"]).unwrap();
    assert_eq!(json["evidence"]["their_region"], "KS");
    assert_eq!(json["evidence"]["their_county"], "SB");
    assert_eq!(
        exported.additional_fields["APP_PANCETTA_CONTEST_CONTACT_ID"],
        "9b16f2be-7d63-4a89-b789-f863df27bccf"
    );
}

fn context() -> pancetta_qso::contest::scoring::ContestScoringContext {
    use pancetta_qso::contest::scoring::*;
    let start = "2026-08-29T14:00:00Z".parse().unwrap();
    let end = "2026-08-30T02:00:00Z".parse().unwrap();
    ContestScoringContext::new(
        "test",
        "us-state-qso-party",
        KSQP_FT_2026,
        "TX",
        vec![ContestPeriod { start, end }],
    )
    .unwrap()
}

fn completed(metadata: pancetta_qso::states::QsoMetadata) -> pancetta_qso::QsoProgress {
    use pancetta_qso::states::*;
    QsoProgress {
        state: QsoState::Completed {
            their_callsign: "K0KS".into(),
            their_report: -10,
            our_report: -10,
            frequency: metadata.frequency,
            grid_square: None,
            completed_at: metadata.start_time,
            duration_seconds: 0,
        },
        metadata,
        state_history: vec![],
        messages: vec![],
    }
}

#[tokio::test]
async fn cold_rebuild_and_deletion_recover_stable_facts() {
    use pancetta_qso::async_database::QsoDatabase;
    use pancetta_qso::contest::ledger::*;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qsos.adi");
    std::fs::write(&path, app_record(1)).unwrap();
    let db = QsoDatabase::replay_from_adif(":memory:", &path)
        .await
        .unwrap();
    let first = db.contest_ledger(&context(), "K5ARH").await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].their_county.as_deref(), Some("SB"));
    let rebuilt = QsoDatabase::replay_from_adif(":memory:", &path)
        .await
        .unwrap();
    assert_eq!(
        first,
        rebuilt.contest_ledger(&context(), "K5ARH").await.unwrap()
    );
    // Database UUIDs can change on import, but the contest UUID cannot.
    let p = AdifProcessor::new();
    let file = p.parse_string(&app_record(1)).unwrap();
    let mut metadata = p.adif_to_qso(&p.record_to_qso(&file.records[0]).unwrap());
    metadata
        .tags
        .insert(CONTACT_ID.into(), uuid::Uuid::new_v4().to_string());
    let progress = completed(metadata);
    db.insert_qso(&progress).await.unwrap();
    assert_eq!(
        db.contest_ledger(&context(), "K5ARH").await.unwrap().len(),
        2
    );
    db.delete_qso(progress.metadata.qso_id).await.unwrap();
    assert_eq!(first, db.contest_ledger(&context(), "K5ARH").await.unwrap());
}

#[tokio::test]
async fn strict_ledger_exceeds_search_page_and_rejects_identity_conflicts() {
    use pancetta_qso::async_database::QsoDatabase;
    use pancetta_qso::contest::ledger::*;
    let db = QsoDatabase::new_in_memory().await.unwrap();
    let p = AdifProcessor::new();
    let file = p.parse_string(&app_record(1)).unwrap();
    let metadata = p.adif_to_qso(&p.record_to_qso(&file.records[0]).unwrap());
    for _ in 0..1100 {
        let mut row = metadata.clone();
        row.qso_id = uuid::Uuid::new_v4();
        row.tags.insert(CONTACT_ID.into(), row.qso_id.to_string());
        db.insert_qso(&completed(row)).await.unwrap();
    }
    assert_eq!(
        db.contest_ledger(&context(), "K5ARH").await.unwrap().len(),
        1100
    );
    let mut changed = context();
    changed.periods[0].start += chrono::Duration::days(365);
    changed.periods[0].end += chrono::Duration::days(365);
    assert!(db
        .contest_ledger(&changed, "K5ARH")
        .await
        .unwrap_err()
        .to_string()
        .contains("instance_id conflicts"));
    assert!(db.contest_ledger(&context(), "W1OTHER").await.is_err());
}

#[tokio::test]
async fn unknown_and_malformed_contexts_round_trip_but_make_ledger_unavailable() {
    use pancetta_qso::async_database::QsoDatabase;
    use pancetta_qso::contest::ledger::*;
    let p = AdifProcessor::new();
    for raw in [app_record(9), app_record(1)] {
        let file = p.parse_string(&raw).unwrap();
        let mut adif = p.record_to_qso(&file.records[0]).unwrap();
        if raw == app_record(1) {
            adif.additional_fields
                .insert(CONTEXT.into(), "bad json".into());
        }
        let metadata = p.adif_to_qso(&adif);
        let exported = p.qso_to_adif(&metadata, metadata.contest_info.as_ref());
        for key in [INSTANCE, RULES, CONTEXT, CONTACT_ID] {
            assert_eq!(adif.additional_fields[key], exported.additional_fields[key]);
        }
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_qso(&completed(metadata)).await.unwrap();
        assert!(db.contest_ledger(&context(), "K5ARH").await.is_err());
    }
}

#[test]
fn conflicting_evidence_is_pending_and_never_guessed_from_grid() {
    use pancetta_qso::contest::ledger::*;
    let p = AdifProcessor::new();
    let file = p.parse_string(&app_record(1)).unwrap();
    let mut adif = p.record_to_qso(&file.records[0]).unwrap();
    adif.state = Some("SC".into());
    let metadata = p.adif_to_qso(&adif);
    let f = fact(&metadata, &context(), "K5ARH").unwrap();
    assert!(f.their_region.is_none());
    assert!(f.their_county.is_none());
    let exported = p.qso_to_adif(&metadata, metadata.contest_info.as_ref());
    assert_eq!(
        adif.additional_fields[CONTEXT],
        exported.additional_fields[CONTEXT]
    );
    adif.state = None;
    adif.additional_fields.remove("CNTY");
    adif.gridsquare = Some("EM10".into());
    let metadata = p.adif_to_qso(&adif);
    let f = fact(&metadata, &context(), "K5ARH").unwrap();
    assert!(f.their_region.is_none());
    assert!(f.their_county.is_none());
}

#[tokio::test]
async fn configured_manual_attempts_enroll_but_autonomous_and_outside_attempts_do_not() {
    use pancetta_qso::contest::{ledger::*, scoring::*};
    use pancetta_qso::{QsoManager, QsoManagerConfig};
    let mut active = context();
    active.periods[0].start = chrono::Utc::now() - chrono::Duration::minutes(1);
    active.periods[0].end = chrono::Utc::now() + chrono::Duration::minutes(10);
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
        contest_scoring: Some(active.clone()),
        ..Default::default()
    });
    let id = manager
        .respond_to_cq_manual("K0KS".into(), 1200.0, None)
        .await
        .unwrap();
    let row = manager.get_qso(id).await.unwrap();
    assert_eq!(row.metadata.tags[INSTANCE], "test");
    assert_eq!(row.metadata.tags[CONTACT_ID], id.to_string());
    assert!(row
        .metadata
        .contest_info
        .as_ref()
        .unwrap()
        .scoring
        .is_some());
    let id = manager
        .respond_to_cq("W1AUTO".into(), 1300.0, None)
        .await
        .unwrap();
    assert!(manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .contest_info
        .is_none());
    active.periods[0].start += chrono::Duration::days(1);
    active.periods[0].end += chrono::Duration::days(1);
    let outside = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
        contest_scoring: Some(active),
        ..Default::default()
    });
    let id = outside
        .respond_to_cq_manual("K0KS".into(), 1200.0, None)
        .await
        .unwrap();
    assert!(outside
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .contest_info
        .is_none());
    // SC has identity for scoring, but must not engage the grid matcher.
    let mut sc = context();
    sc.rules_id = SCQP_DIGITAL_FIXED_2026.into();
    sc.periods[0].start = chrono::Utc::now() - chrono::Duration::minutes(1);
    sc.periods[0].end = chrono::Utc::now() + chrono::Duration::minutes(10);
    let sc_manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
        contest_scoring: Some(sc),
        ..Default::default()
    });
    let id = sc_manager
        .start_cq_manual(1200.0, None, false, None)
        .await
        .unwrap();
    let row = sc_manager.get_qso(id).await.unwrap();
    assert_eq!(row.metadata.tags[RULES], SCQP_DIGITAL_FIXED_2026);
    assert!(row
        .metadata
        .contest_info
        .as_ref()
        .unwrap()
        .scoring
        .is_some());
    let id = sc_manager
        .respond_to_cq_manual("K0SC".into(), 1200.0, None)
        .await
        .unwrap();
    let before = sc_manager.get_qso(id).await.unwrap().state;
    sc_manager
        .process_message(
            pancetta_qso::states::MessageType::NonStandard {
                text: "K5ARH K0SC R EM40".into(),
            },
            "K5ARH K0SC R EM40".into(),
            1200.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    assert_eq!(before, sc_manager.get_qso(id).await.unwrap().state);
}

#[test]
fn fractional_live_start_survives_adif_round_trip() {
    use pancetta_qso::contest::ledger::*;
    let p = AdifProcessor::new();
    let file = p.parse_string(&app_record(1)).unwrap();
    let mut metadata = p.adif_to_qso(&p.record_to_qso(&file.records[0]).unwrap());
    metadata.our_callsign = "K5ARH".into();
    metadata.start_time = "2026-08-29T14:30:00.123456Z".parse().unwrap();
    attach(&mut metadata, &context());
    let before = fact(&metadata, &context(), "K5ARH").unwrap();
    let raw = p
        .generate_record(&p.qso_to_adif(&metadata, metadata.contest_info.as_ref()))
        .unwrap();
    let file = p.parse_string(&raw).unwrap();
    let metadata = p.adif_to_qso(&p.record_to_qso(&file.records[0]).unwrap());
    assert_eq!(before, fact(&metadata, &context(), "K5ARH").unwrap());
}
