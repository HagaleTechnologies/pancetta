use super::{prepare_local_qso_index, start_local_qso_log_writers, AdifSubscribers, LocalIndex};
use std::time::Duration;

fn adif_backup_copies(directory: &std::path::Path) -> Vec<Vec<u8>> {
    if !directory.exists() {
        return Vec::new();
    }
    let mut paths: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("qsos_adi_")
        })
        .collect();
    paths.sort();
    paths.iter().map(|p| std::fs::read(p).unwrap()).collect()
}

async fn adif_backup_start(
    root: &std::path::Path,
    replay: bool,
    settings: &pancetta_config::DatabaseConfig,
) -> bool {
    // A private runtime per simulated station also stops the logger's
    // detached tasks and closes their database pools between starts.
    let root = root.to_owned();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let subscribers = AdifSubscribers::new();
            let (writer, logger, seed) = start_local_qso_log_writers(
                replay,
                &root.join("qsos.adi"),
                &root.join("qso.db"),
                100,
                false,
                &settings,
                &manager(),
                &subscribers,
            )
            .await;
            if !replay {
                assert!(
                    logger.is_some(),
                    "backup failure must not prevent index logger initialization"
                );
            }
            let opened = writer.is_some();
            subscribers.stop(Duration::from_secs(2)).await;
            drop(writer);
            drop(logger);
            if let LocalIndex::Ready(db) = seed {
                db.close().await;
            }
            opened
        })
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn live_startup_backs_up_changed_adif() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("qsos.adi");
    let mut expected = Vec::new();
    for bytes in [TWO_RECORDS.to_owned(), format!("{TWO_RECORDS}\n")] {
        std::fs::write(&source, bytes.as_bytes()).unwrap();
        assert!(adif_backup_start(dir.path(), false, &Default::default()).await);
        expected.push(bytes.as_bytes().to_vec());
        assert_eq!(adif_backup_copies(&dir.path().join("backups")), expected);
        assert_eq!(std::fs::read(&source).unwrap(), bytes.as_bytes());
    }
}

#[tokio::test]
async fn live_startup_skips_unchanged_adif() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("qsos.adi"), TWO_RECORDS).unwrap();
    for _ in 0..2 {
        assert!(adif_backup_start(dir.path(), false, &Default::default()).await);
    }
    assert_eq!(
        adif_backup_copies(&dir.path().join("backups")),
        vec![TWO_RECORDS.as_bytes()]
    );
}

#[tokio::test]
async fn live_startup_honors_adif_backup_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = pancetta_config::DatabaseConfig {
        adif_backup_directory: "custom".into(),
        adif_backup_retain_count: 1,
        ..Default::default()
    };
    for bytes in [TWO_RECORDS.to_owned(), format!("{TWO_RECORDS}\n")] {
        std::fs::write(dir.path().join("qsos.adi"), bytes.as_bytes()).unwrap();
        assert!(adif_backup_start(dir.path(), false, &settings).await);
        assert_eq!(
            adif_backup_copies(&dir.path().join("custom")),
            vec![bytes.as_bytes()]
        );
    }
    assert!(!dir.path().join("backups").exists());
    settings.adif_backup_enabled = false;
    settings.adif_backup_directory = "disabled".into();
    assert!(adif_backup_start(dir.path(), false, &settings).await);
    assert!(!dir.path().join("disabled").exists());
}

#[tokio::test]
async fn startup_adif_backup_skip_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("qsos.adi"), TWO_RECORDS).unwrap();
    assert!(!adif_backup_start(dir.path(), true, &Default::default()).await);
    assert!(!dir.path().join("backups").exists());
    let missing = tempfile::tempdir().unwrap();
    assert!(adif_backup_start(missing.path(), false, &Default::default()).await);
    assert!(!missing.path().join("backups").exists());
    // The migration-refusal test below verifies refusal with a corrupt legacy row.
}

#[tokio::test]
async fn startup_adif_backup_failure_is_nonfatal() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("qsos.adi");
    std::fs::write(&source, TWO_RECORDS).unwrap();
    assert!(adif_backup_start(dir.path(), false, &Default::default()).await);
    let before = adif_backup_copies(&dir.path().join("backups"));
    std::fs::write(&source, format!("{TWO_RECORDS}\n")).unwrap();
    let locked_source = source.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let lock = pancetta_qso::logbook_lock::LogbookLock::open(&locked_source).unwrap();
        let _held = lock.try_lock().unwrap().unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
    });
    ready_rx.recv().unwrap();
    assert!(adif_backup_start(dir.path(), false, &Default::default()).await);
    holder.join().unwrap();
    assert_eq!(adif_backup_copies(&dir.path().join("backups")), before);
    std::fs::write(dir.path().join("blocked"), b"file").unwrap();
    let settings = pancetta_config::DatabaseConfig {
        adif_backup_directory: "blocked".into(),
        ..Default::default()
    };
    assert!(adif_backup_start(dir.path(), false, &settings).await);
    assert_eq!(adif_backup_copies(&dir.path().join("backups")), before);
}

/// Minimal manager config: a real callsign (the placeholder guard in
/// `start_cq` rejects NOCALL/N0CALL) and a grid, everything else default.
fn manager() -> pancetta_qso::QsoManager {
    pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        ..Default::default()
    })
}

/// Drive one full CQ→grid→report→73 exchange, which is what makes the
/// manager emit `QsoEvent::QsoCompleted` — the single event both local
/// writers subscribe to.
pub(super) async fn complete_one_qso(manager: &pancetta_qso::QsoManager) {
    let freq = 14_074_000.0;
    manager
        .start_cq(freq, None, false, None)
        .await
        .expect("start_cq should succeed");

    manager
        .process_message(
            pancetta_qso::MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .expect("CqResponse should be accepted");
    manager
        .process_message(
            pancetta_qso::MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .expect("ReportAck should be accepted");
    manager
        .process_message(
            pancetta_qso::MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .expect("73 should be accepted");
}

/// Number of complete ADIF records in `path` (`<eor>` terminates one).
/// A missing file counts as zero — the file only exists once something
/// opened it for writing.
pub(super) fn adif_record_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .matches("<eor>")
        .count()
}

/// Poll for up to ~5s: the ADIF append happens in the subscriber task, so
/// the LIVE control has to wait for it rather than race it.
pub(super) async fn wait_for_adif_record(path: &std::path::Path) -> usize {
    for _ in 0..100 {
        let n = adif_record_count(path);
        if n > 0 {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    adif_record_count(path)
}

/// Two pancetta-style 20 m records; FREQ is what history seeding bands by.
const TWO_RECORDS: &str = "pancetta test log <ADIF_VER:5>3.1.0 <EOH>\n\
    <CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
    <FREQ:9>14.074000 <BAND:3>20M <EOR>\n\
    <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <MODE:3>FT8 \
    <FREQ:9>14.074000 <BAND:3>20M <EOR>\n";

#[tokio::test]
async fn pan298_logged_comment_follows_attribution_setting() {
    for enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let adif = dir.path().join("qsos.adi");
        let manager = manager();
        let subscribers = AdifSubscribers::new();
        let (_writer, logger, seed) = start_local_qso_log_writers(
            false,
            &adif,
            &dir.path().join("qso.db"),
            100,
            enabled,
            &pancetta_config::DatabaseConfig::default(),
            &manager,
            &subscribers,
        )
        .await;
        complete_one_qso(&manager).await;
        assert_eq!(wait_for_adif_record(&adif).await, 1);
        let p = pancetta_qso::AdifProcessor::new();
        let text = std::fs::read_to_string(&adif).unwrap();
        let parsed = p.parse_string(&text).unwrap();
        let qso = p.record_to_qso(&parsed.records[0]).unwrap();
        assert_eq!(qso.comment.as_deref(), enabled.then_some("Using cqdx.io & Pancetta -- https://cqdx.io https://github.com/HagaleTechnologies/pancetta"));
        subscribers.stop(Duration::from_secs(2)).await;
        drop(logger);
        seed.expect("seed").close().await;
    }
}

#[tokio::test]
async fn live_start_does_not_rebuild_after_a_kill_with_rows_in_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let killed = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let path = dir.path().join("qso.db");
    let manager = manager();
    let subscribers = AdifSubscribers::new();
    let (_writer, logger, seed) = start_local_qso_log_writers(
        false,
        &adif,
        &path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &subscribers,
    )
    .await;
    let db = seed.expect("seed");
    complete_one_qso(&manager).await;
    assert_eq!(wait_for_adif_record(&adif).await, 1);
    for _ in 0..100 {
        if db.count_qsos().await.unwrap() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(db.count_qsos().await.unwrap(), 1);
    let ids = qso_ids(&db).await;
    for name in ["qsos.adi", "qso.db", "qso.db-wal", "qso.db-shm"] {
        let source = dir.path().join(name);
        if source.exists() {
            let target = killed.path().join(name);
            std::fs::copy(&source, &target).unwrap();
            set_mtime(
                &target,
                std::fs::metadata(source).unwrap().modified().unwrap(),
            );
        }
    }
    let recovered = prepare_local_qso_index(
        &killed.path().join("qsos.adi"),
        &killed.path().join("qso.db"),
    )
    .await
    .expect("recovered history");
    assert_eq!(qso_ids(&recovered).await, ids);
    recovered.close().await;
    subscribers.stop(Duration::from_secs(2)).await;
    drop(logger);
    db.close().await;
}

async fn qso_ids(db: &pancetta_qso::async_database::QsoDatabase) -> Vec<String> {
    let mut ids: Vec<_> = db
        .search_qsos(&Default::default(), &Default::default())
        .await
        .unwrap()
        .iter()
        .map(|q| q.metadata.qso_id.to_string())
        .collect();
    ids.sort();
    ids
}
fn set_mtime(path: &std::path::Path, at: std::time::SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}
#[tokio::test]
async fn live_start_rebuilds_an_index_whose_log_grew_at_the_same_mtime() {
    grown_log(false).await;
}
#[tokio::test]
async fn live_start_rebuilds_an_index_newer_than_a_grown_log() {
    grown_log(true).await;
}
async fn grown_log(newer: bool) {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let path = dir.path().join("qso.db");
    index_with_two_rows(&path).await;
    std::fs::write(&adif, format!("{TWO_RECORDS}<CALL:5>W2XYZ <QSO_DATE:8>20240102 <TIME_ON:6>120000 <FREQ:9>14.074000 <MODE:3>FT8 <EOR>")).unwrap();
    let now = std::time::SystemTime::now();
    set_mtime(&adif, now - Duration::from_secs(120));
    set_mtime(
        &path,
        now - Duration::from_secs(if newer { 60 } else { 120 }),
    );
    let db = prepare_local_qso_index(&adif, &path)
        .await
        .expect("rebuilt");
    assert_eq!(db.count_qsos().await.unwrap(), 3);
    db.close().await;
}
#[tokio::test]
async fn live_start_keeps_a_copy_of_an_index_holding_contacts_the_log_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let path = dir.path().join("qso.db");
    index_with_two_rows(&path).await;
    std::fs::write(&adif, TWO_RECORDS.split_once("<CALL:5>K1ABC").unwrap().0).unwrap();
    let db = prepare_local_qso_index(&adif, &path)
        .await
        .expect("rebuilt");
    assert_eq!(db.count_qsos().await.unwrap(), 1);
    db.close().await;
    let old =
        pancetta_qso::async_database::QsoDatabase::open(dir.path().join("qso.db.pre-rebuild"))
            .await
            .unwrap();
    assert_eq!(old.count_qsos().await.unwrap(), 2);
    old.close().await;
}
#[tokio::test]
async fn live_start_rebuilds_an_unreadable_index_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let path = dir.path().join("qso.db");
    std::fs::write(&path, b"this is not a database").unwrap();
    std::fs::write(dir.path().join("qso.db-wal"), [42; 64]).unwrap();
    std::fs::write(&adif, TWO_RECORDS).unwrap();
    let db = prepare_local_qso_index(&adif, &path)
        .await
        .expect("rebuilt");
    assert_eq!(db.count_qsos().await.unwrap(), 2);
    db.close().await;
    assert_eq!(
        std::fs::read(dir.path().join("qso.db.pre-rebuild-wal")).unwrap(),
        [42; 64]
    );
    assert_eq!(
        std::fs::read(dir.path().join("qso.db.pre-rebuild")).unwrap(),
        b"this is not a database"
    );
}
#[tokio::test]
async fn live_start_refuses_a_legacy_migration_with_an_unreadable_row() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("qsos.adi");
    let path = dir.path().join("qso.db");
    index_with_two_rows(&path).await;
    use sqlx::Connection;
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE qsos SET progress_data = '{not json' WHERE id=1")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    for _ in 0..2 {
        let subscribers = AdifSubscribers::new();
        let (writer, logger, outcome) = start_local_qso_log_writers(
            false,
            &adif,
            &path,
            100,
            false,
            &pancetta_config::DatabaseConfig::default(),
            &manager(),
            &subscribers,
        )
        .await;
        assert!(writer.is_none());
        match outcome {
            LocalIndex::MigrationRefused { reason } => assert!(
                reason.contains("row 1") && reason.contains("qso.db.pre-migration"),
                "{reason}"
            ),
            _ => panic!("migration must refuse"),
        }
        assert!(!adif.exists());
        assert!(!adif.parent().unwrap().join("backups").exists());
        subscribers.stop(Duration::from_secs(2)).await;
        drop(logger);
        let db = pancetta_qso::async_database::QsoDatabase::open(&path)
            .await
            .unwrap();
        assert_eq!(db.count_qsos().await.unwrap(), 2);
        db.close().await;
    }
}

/// Build a 2-row `qso.db` at `db_path` from an ADIF kept somewhere else,
/// so the directory under test holds the index alone.
async fn index_with_two_rows(db_path: &std::path::Path) {
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let source = elsewhere.path().join("source.adi");
    std::fs::write(&source, TWO_RECORDS).expect("write source ADIF");
    let db = pancetta_qso::async_database::QsoDatabase::replay_from_adif(db_path, &source)
        .await
        .expect("build legacy index");
    assert_eq!(db.count_qsos().await.expect("count"), 2, "precondition");
    db.close().await;
}

/// PAN-127: a legacy install (`qso.db`, no `qsos.adi`) used to be wiped,
/// because the ADIF writer created an empty `qsos.adi` first, which made
/// the index look stale and replayed it from zero records.
#[tokio::test]
async fn live_start_migrates_a_legacy_index_instead_of_wiping_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    index_with_two_rows(&db_path).await;
    let original = pancetta_qso::async_database::QsoDatabase::open(&db_path)
        .await
        .unwrap();
    let ids = qso_ids(&original).await;
    original.close().await;
    assert!(!adif_path.exists(), "precondition: no ADIF log yet");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;

    let seed_db = seed_db.expect("a live start returns the seed index");
    assert_eq!(
        seed_db.count_qsos().await.expect("count"),
        2,
        "the legacy contacts must survive startup"
    );
    assert!(dir.path().join("qso.db.pre-migration").exists());
    assert_eq!(qso_ids(&seed_db).await, ids);
    let adif = std::fs::read_to_string(&adif_path).expect("qsos.adi was written");
    let parsed = pancetta_qso::AdifProcessor::new()
        .parse_string(&adif)
        .expect("migrated ADIF parses");
    assert_eq!(parsed.records.len(), 2, "both contacts migrated to ADIF");
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// PAN-127: a `qsos.adi` with no `qso.db` (dropped in, or the index
/// deleted to force a rebuild) used to be left unindexed, because the
/// SQLite logger created an empty `qso.db` that then looked current.
#[tokio::test]
async fn live_start_rebuilds_a_missing_index_from_the_adif_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    std::fs::write(&adif_path, TWO_RECORDS).expect("write qsos.adi");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;

    let seed_db = seed_db.expect("a live start returns the seed index");
    let mut calls = seed_db.get_worked_callsigns("20M").await;
    calls.sort();
    assert_eq!(calls, vec!["K1ABC".to_string(), "W1AW".to_string()]);
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// An index newer than the ADIF log is opened as it is, not rebuilt.
#[tokio::test]
async fn live_start_opens_a_current_index_as_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    std::fs::write(&adif_path, TWO_RECORDS).expect("write qsos.adi");
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&adif_path)
        .and_then(|f| f.set_modified(an_hour_ago))
        .expect("backdate qsos.adi");
    index_with_two_rows(&db_path).await;
    set_mtime(&db_path, an_hour_ago);
    set_mtime(&adif_path, std::time::SystemTime::now());
    let original = pancetta_qso::async_database::QsoDatabase::open(&db_path)
        .await
        .unwrap();
    let ids = qso_ids(&original).await;
    original.close().await;
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;

    let seed_db = seed_db.expect("a live start returns the seed index");
    assert_eq!(
        seed_db.count_qsos().await.expect("count"),
        2,
        "a current index must not be rebuilt"
    );
    assert_eq!(qso_ids(&seed_db).await, ids);
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// PAN-281 scenario 2: a power cut right after a completion keeps the
/// synced ADIF record but can lose the index's row (its WAL was not yet
/// synced, and a WAL commit never moves `qso.db`'s mtime). The restart
/// must index the QSO the ADIF holds. The post-cut state is built in a
/// second directory: session 1's real `qsos.adi`, and an older, empty
/// `qso.db`. It holds under the mtime rule and a content rule alike.
#[tokio::test]
async fn live_start_indexes_a_logged_qso_the_index_lost() {
    // Session 1: a live run logs one QSO with K1DEF.
    let before_cut = tempfile::tempdir().expect("tempdir");
    let logged_adif = before_cut.path().join("qsos.adi");
    let first_session = manager();
    let adif_subscribers = AdifSubscribers::new();
    let _session_one = start_local_qso_log_writers(
        false,
        &logged_adif,
        &before_cut.path().join("qso.db"),
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &first_session,
        &adif_subscribers,
    )
    .await;
    complete_one_qso(&first_session).await;
    assert_eq!(
        wait_for_adif_record(&logged_adif).await,
        1,
        "precondition: the completed QSO is in qsos.adi"
    );
    adif_subscribers.stop(Duration::from_secs(2)).await;

    // The state after the cut: the ADIF kept the record, the index did
    // not, and the index file is older than the ADIF.
    let after_cut = tempfile::tempdir().expect("tempdir");
    let adif_path = after_cut.path().join("qsos.adi");
    let db_path = after_cut.path().join("qso.db");
    std::fs::copy(&logged_adif, &adif_path).expect("copy qsos.adi");
    pancetta_qso::async_database::QsoDatabase::open(&db_path)
        .await
        .expect("create the empty index")
        .close()
        .await;
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&db_path)
        .and_then(|f| f.set_modified(an_hour_ago))
        .expect("backdate qso.db");

    // Restart on the post-cut state.
    let restarted = manager();
    let adif_subscribers = AdifSubscribers::new();
    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &restarted,
        &adif_subscribers,
    )
    .await;

    let seed_db = seed_db.expect("a live start returns the seed index");
    assert_eq!(
        seed_db.get_worked_callsigns("20M").await,
        vec!["K1DEF".to_string()],
        "the restart must index the QSO the ADIF kept"
    );
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// PAN-127: an import stopped part-way leaves a partial index that is
/// newer than the ADIF log, plus the rebuild marker. The marker, not the
/// mtimes, decides: the index is rebuilt and the marker cleared.
#[tokio::test]
async fn live_start_rebuilds_an_index_an_interrupted_import_marked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    std::fs::write(&adif_path, TWO_RECORDS).expect("write qsos.adi");
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&adif_path)
        .and_then(|f| f.set_modified(an_hour_ago))
        .expect("backdate qsos.adi");
    // The partial index: the first contact only.
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let first = elsewhere.path().join("first.adi");
    let one_record = TWO_RECORDS
        .split_once("<CALL:5>K1ABC")
        .map(|(head, _)| head)
        .expect("fixture has two records");
    std::fs::write(&first, one_record).expect("write first.adi");
    pancetta_qso::async_database::QsoDatabase::replay_from_adif(&db_path, &first)
        .await
        .expect("build partial index")
        .close()
        .await;
    let marker = pancetta_qso::async_database::QsoDatabase::rebuild_marker(&db_path);
    std::fs::write(&marker, b"").expect("write marker");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;

    let seed_db = seed_db.expect("a live start returns the seed index");
    assert_eq!(
        seed_db.count_qsos().await.expect("count"),
        2,
        "a marked index must be rebuilt from the ADIF log"
    );
    assert!(!marker.exists(), "the rebuild clears the marker");
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// PR #451 review (Codex P2): a replay that fails (here, on an
/// unreadable record) keeps the marker, so the rebuild is retried at the
/// next start once the cause is fixed, instead of the stale index being
/// trusted from then on. The failed replay leaves the old index in place.
#[tokio::test]
async fn live_start_keeps_the_marker_when_the_rebuild_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    index_with_two_rows(&db_path).await;
    std::fs::write(&adif_path, "<CALL:4>W1AW <TIME_ON:x>120000 <EOR>\n").expect("write qsos.adi");
    let marker = pancetta_qso::async_database::QsoDatabase::rebuild_marker(&db_path);
    std::fs::write(&marker, b"").expect("write marker");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, _async_logger, seed_db) = start_local_qso_log_writers(
        false,
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;

    assert!(marker.exists(), "a failed rebuild keeps the marker");
    assert!(
        seed_db.is_none(),
        "failed rebuild must not publish stale history"
    );
    let retained = pancetta_qso::async_database::QsoDatabase::open(&db_path)
        .await
        .unwrap();
    assert_eq!(retained.count_qsos().await.expect("count"), 2);
    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// CONTROL (makes the replay assertion below non-vacuous): on a live run
/// the very same exchange DOES reach both local writers — the ADIF file
/// gains a record and the SQLite index is created.
#[tokio::test]
async fn live_run_writes_the_completed_qso_to_the_local_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (adif_writer, async_logger, _seed_db) = start_local_qso_log_writers(
        false, // NOT replay
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;
    assert!(
        adif_writer.is_some(),
        "precondition: a live run must open the ADIF source of truth"
    );
    assert!(
        async_logger.is_some(),
        "precondition: a live run must construct the SQLite QSO logger"
    );

    complete_one_qso(&manager).await;

    assert_eq!(
        wait_for_adif_record(&adif_path).await,
        1,
        "precondition: a completed QSO must be appended to the ADIF log on a \
         live run — otherwise the --replay assertions prove nothing"
    );
    assert!(
        db_path.exists(),
        "precondition: a live run must create the SQLite index at {}",
        db_path.display()
    );

    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// Automatic backups land in `backups/` next to `qso.db`
/// (`~/.pancetta/backups`), never under the process cwd.
#[tokio::test]
async fn live_logger_backs_up_next_to_the_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("qso.db");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (_adif_writer, async_logger, _seed_db) = start_local_qso_log_writers(
        false,
        &dir.path().join("qsos.adi"),
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;
    let backup = async_logger
        .expect("live run constructs the logger")
        .create_backup()
        .await
        .expect("backup succeeds");
    assert_eq!(backup.parent().unwrap(), dir.path().join("backups"));

    adif_subscribers.stop(Duration::from_secs(2)).await;
}

/// The gate itself: under `--replay` a QSO the engine "completes" off
/// replayed (historical) traffic must leave ZERO trace in the operator's
/// local log. Not a tagged record — no record, and no file at all, because
/// ADIF has no portable way to mark a record as synthetic.
#[tokio::test]
async fn replay_run_writes_nothing_to_the_local_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    let manager = manager();
    let adif_subscribers = AdifSubscribers::new();

    let (adif_writer, async_logger, seed_db) = start_local_qso_log_writers(
        true, // --replay
        &adif_path,
        &db_path,
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &manager,
        &adif_subscribers,
    )
    .await;
    assert!(
        adif_writer.is_none(),
        "the ADIF source of truth must not be opened under --replay"
    );
    assert!(
        async_logger.is_none(),
        "the SQLite QSO logger must not be constructed under --replay"
    );
    assert!(
        seed_db.is_none(),
        "the real index must not be opened (or rebuilt) under --replay"
    );

    complete_one_qso(&manager).await;

    // Give any (wrongly) spawned subscriber the same window the live
    // control needed to land its record, so this is a real wait, not an
    // instant pass on a race.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_eq!(
        adif_record_count(&adif_path),
        0,
        "a replayed QSO must not appear in the ADIF log"
    );
    assert!(
        !adif_path.exists(),
        "--replay must not even create {} — zero trace",
        adif_path.display()
    );
    assert!(
        !db_path.exists(),
        "--replay must not even create the SQLite index at {} — zero trace",
        db_path.display()
    );

    adif_subscribers.stop(Duration::from_secs(2)).await;
}
