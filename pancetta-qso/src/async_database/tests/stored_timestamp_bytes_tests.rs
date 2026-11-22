//! PAN-441: rows the chrono build wrote to `qso.db` keep their exact bytes
//! across the chrono → jiff migration. `qso_rows.sql` was dumped once from
//! the chrono build's `insert_qso_with`; every expected string here is a
//! literal, never re-blessed.

use super::*;
use pancetta_core::time::UtcTimestamp;

const FIXTURE_ROWS: &str = include_str!("../../../tests/fixtures/pan441/qso_rows.sql");

/// 2026-11-04T14:30:00Z.
const BASE_SECS: i64 = 1_793_802_600;

/// The fixture rows: callsign, qso_id and the sub-second part of both
/// `metadata.start_time` and `created_at`.
const ROWS: [(&str, u128, u32); 4] = [
    ("K1AAA", 0x4410_0001, 0),
    ("K1BBB", 0x4410_0002, 500_000_000),
    ("K1CCC", 0x4410_0003, 510_000_000),
    ("K1DDD", 0x4410_0004, 123_456_780),
];

const CREATED_AT_TEXT: [&str; 4] = [
    "2026-11-04T14:30:00+00:00",
    "2026-11-04T14:30:00.500+00:00",
    "2026-11-04T14:30:00.510+00:00",
    "2026-11-04T14:30:00.123456780+00:00",
];

const UPDATED_AT_TEXT: &str = "2026-11-21T21:44:57.519597210+00:00";

fn instant(nanos: u32) -> UtcTimestamp {
    UtcTimestamp::from_parts(BASE_SECS, nanos).expect("in range")
}

fn qso_id(raw: u128) -> QsoId {
    Uuid::from_u128(raw)
}

fn fixture_progress(callsign: &str, id: u128, at: UtcTimestamp) -> QsoProgress {
    let frequency = 14_074_000.0;
    QsoProgress {
        state: QsoState::Completed {
            their_callsign: callsign.into(),
            their_report: -10,
            our_report: -12,
            frequency,
            grid_square: None,
            completed_at: at,
            duration_seconds: 60,
        },
        state_history: vec![],
        messages: vec![],
        metadata: QsoMetadata {
            qso_id: qso_id(id),
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some(callsign.to_string()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: at,
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

/// A file database (so the pool's five connections share one database)
/// holding the chrono build's four rows.
async fn fixture_db() -> (tempfile::TempDir, QsoDatabase) {
    let tmp = tempfile::tempdir().unwrap();
    let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();
    sqlx::query(FIXTURE_ROWS).execute(&db.pool).await.unwrap();
    (tmp, db)
}

type StoredRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

async fn stored_rows(db: &QsoDatabase) -> Vec<StoredRow> {
    sqlx::query_as(
        "SELECT qso_id, metadata, final_state, progress_data, adif_data, created_at, \
         updated_at, checksum FROM qsos ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn sqlite_fixture_rows_reserialize_byte_for_byte() {
    let (_tmp, db) = fixture_db().await;
    let rows = stored_rows(&db).await;
    assert_eq!(rows.len(), 4);

    let records = db
        .search_qsos_records(&QsoFilter::default(), &QueryOptions::default())
        .await
        .unwrap();
    assert_eq!(records.len(), 4, "every fixture row reads back");

    for (i, (id, metadata, final_state, progress_data, adif_data, created_at, updated_at, _)) in
        rows.iter().enumerate()
    {
        let (callsign, raw_id, nanos) = ROWS[i];
        assert_eq!(id, &qso_id(raw_id).to_string());
        assert_eq!(created_at, CREATED_AT_TEXT[i], "{callsign} created_at text");
        assert_eq!(updated_at, UPDATED_AT_TEXT, "{callsign} updated_at text");

        let progress = db.get_qso(qso_id(raw_id)).await.unwrap();
        assert_eq!(progress.metadata.start_time, instant(nanos));
        assert_eq!(&serde_json::to_string(&progress).unwrap(), progress_data);
        assert_eq!(
            &serde_json::to_string(&progress.metadata).unwrap(),
            metadata
        );
        assert_eq!(
            &serde_json::to_string(&progress.state).unwrap(),
            final_state
        );

        let record = records
            .iter()
            .find(|r| r.qso_id == qso_id(raw_id))
            .expect("record for fixture row");
        assert_eq!(&serde_json::to_string(&record.metadata).unwrap(), metadata);
        assert_eq!(
            &serde_json::to_string(&record.final_state).unwrap(),
            final_state
        );
        assert_eq!(
            &serde_json::to_string(&record.adif_data).unwrap(),
            adif_data
        );
        assert_eq!(&record.created_at.to_rfc3339(), created_at);
        assert_eq!(&record.updated_at.to_rfc3339(), updated_at);
        assert_eq!(record.created_at, instant(nanos));
    }

    // The JSON forms themselves, as the chrono build wrote them.
    let start_times: Vec<String> =
        sqlx::query_scalar("SELECT json_extract(metadata, '$.start_time') FROM qsos ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        start_times,
        vec![
            "2026-11-04T14:30:00Z",
            "2026-11-04T14:30:00.500Z",
            "2026-11-04T14:30:00.510Z",
            "2026-11-04T14:30:00.123456780Z",
        ]
    );
    db.close().await;
}

#[tokio::test]
async fn sqlite_fixture_update_keeps_bytes_and_checksum() {
    let (_tmp, db) = fixture_db().await;
    let before = stored_rows(&db).await;
    let (_, raw_id, _) = ROWS[1];

    let progress = db.get_qso(qso_id(raw_id)).await.unwrap();
    db.update_qso(&progress).await.unwrap();

    let after = stored_rows(&db).await;
    let (_, metadata, final_state, progress_data, adif_data, created_at, _, checksum) = &after[1];
    let (_, b_metadata, b_final_state, b_progress_data, b_adif_data, b_created_at, _, b_checksum) =
        &before[1];
    assert_eq!(metadata, b_metadata);
    assert_eq!(final_state, b_final_state);
    assert_eq!(progress_data, b_progress_data);
    assert_eq!(adif_data, b_adif_data);
    assert_eq!(checksum, b_checksum);
    assert_eq!(created_at, b_created_at);
    assert_eq!(created_at, "2026-11-04T14:30:00.500+00:00");
    db.close().await;
}

#[tokio::test]
async fn sqlite_insert_writes_chrono_column_and_json_forms() {
    let tmp = tempfile::tempdir().unwrap();
    let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();
    let at = instant(500_000_000);
    let progress = fixture_progress("K1EEE", 0x4410_0005, at);
    db.insert_qso_with(&db.pool, &progress, at).await.unwrap();

    let (created_at, start_time): (String, String) = sqlx::query_as(
        "SELECT created_at, json_extract(metadata, '$.start_time') FROM qsos WHERE qso_id = ?",
    )
    .bind(qso_id(0x4410_0005).to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(created_at, "2026-11-04T14:30:00.500+00:00");
    assert_eq!(start_time, "2026-11-04T14:30:00.500Z");
    db.close().await;
}

#[tokio::test]
async fn sqlite_created_at_order_survives_trailing_zero_fractions() {
    let (_tmp, db) = fixture_db().await;
    let filter = QsoFilter {
        date_range: Some(DateRange {
            start: instant(500_000_000),
            end: instant(510_000_000),
        }),
        ..QsoFilter::default()
    };
    let records = db
        .search_qsos_records(&filter, &QueryOptions::default())
        .await
        .unwrap();
    let calls: Vec<Option<String>> = records
        .iter()
        .map(|r| r.metadata.their_callsign.clone())
        .collect();
    assert_eq!(
        calls,
        vec![Some("K1CCC".to_string()), Some("K1BBB".to_string())],
        "the .510 row sorts after (DESC: before) the .500 row"
    );

    let stats = db.get_statistics().await.unwrap();
    assert_eq!(stats.first_qso, Some(instant(0)));
    assert_eq!(
        stats.first_qso.map(|t| t.to_rfc3339()),
        Some("2026-11-04T14:30:00+00:00".to_string())
    );
    db.close().await;
}

#[tokio::test]
async fn sqlite_check_duplicate_matches_fixture_fraction() {
    let (_tmp, db) = fixture_db().await;
    let window_hours = 1;
    let one_hour = 3_600;
    let now_for_cutoff =
        |nanos: u32| UtcTimestamp::from_parts(BASE_SECS + one_hour, nanos).expect("in range");

    // Cutoff 2026-11-04T14:30:00.123456779Z: the stored .123456780 start is inside.
    let found = db
        .check_duplicate(
            "K1DDD",
            Some("20m"),
            now_for_cutoff(123_456_779),
            window_hours,
        )
        .await
        .unwrap();
    assert_eq!(found, Some(qso_id(ROWS[3].1)));

    // Cutoff 2026-11-04T14:30:00.123456781Z: the stored start is before it.
    let missed = db
        .check_duplicate(
            "K1DDD",
            Some("20m"),
            now_for_cutoff(123_456_781),
            window_hours,
        )
        .await
        .unwrap();
    assert_eq!(missed, None);
    db.close().await;
}
