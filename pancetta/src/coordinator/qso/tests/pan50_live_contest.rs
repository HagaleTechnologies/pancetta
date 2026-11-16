use super::*;
use pancetta_qso::contest::scoring::*;
#[tokio::test]
async fn manual_completion_survives_real_writers_and_adif_index_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let index = dir.path().join("qso.db");
    let now = chrono::Utc::now();
    let context = ContestScoringContext::new(
        "live",
        "us-state-qso-party",
        KSQP_FT_2026,
        "KS",
        vec![ContestPeriod {
            start: now - chrono::Duration::minutes(1),
            end: now + chrono::Duration::minutes(10),
        }],
    )
    .unwrap();
    let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        our_grid: Some("FN42".into()),
        contest_scoring: Some(context.clone()),
        ..Default::default()
    });
    let adif_subscribers = AdifSubscribers::new();
    let (_writer, _logger, db) = start_local_qso_log_writers(
        false,
        &adif,
        &index,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;
    let db = db.expect("ready history");
    let id = manager
        .start_cq_manual(14_074_000.0, None, false, None)
        .await
        .unwrap();
    use pancetta_qso::MessageType as M;
    for message in [
        M::CqResponse {
            calling_station: "W1ABC".into(),
            responding_station: "K1DEF".into(),
            grid: Some("FN31".into()),
        },
        M::ReportAck {
            to_station: "W1ABC".into(),
            from_station: "K1DEF".into(),
            report: -12,
        },
        M::SeventyThree {
            to_station: "W1ABC".into(),
            from_station: "K1DEF".into(),
        },
    ] {
        manager
            .process_message(message, String::new(), 14_074_000.0, Some(-10.0))
            .await
            .unwrap();
    }
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        pancetta_qso::QsoState::Completed { .. }
    ));
    let facts = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let facts = db.contest_ledger(&context, "W1ABC").await.unwrap();
            if facts.len() == 1
                && tokio::fs::read_to_string(&adif)
                    .await
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains("<eor>")
            {
                break facts;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    adif_subscribers
        .stop(std::time::Duration::from_secs(2))
        .await;
    let rebuilt = pancetta_qso::async_database::QsoDatabase::replay_from_adif(":memory:", &adif)
        .await
        .unwrap();
    assert_eq!(
        facts,
        rebuilt.contest_ledger(&context, "W1ABC").await.unwrap()
    );
    assert_eq!(
        reduce_contest_score(&context, &facts, chrono::Utc::now())
            .unwrap()
            .score,
        1
    );
}
