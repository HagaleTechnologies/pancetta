// PAN-268. Formerly the inline module `pan268`.
use super::*;

// PAN-268: replay startup must hand the contest worker the prepared
// in-memory seed (or its failure), never the real station index.
#[tokio::test]
async fn replay_startup_selects_only_the_prepared_seed_for_the_contest_worker() {
    use super::super::contest::{select_history_source, HistorySource};
    let dir = tempfile::tempdir().unwrap();
    let adif_path = dir.path().join("qsos.adi");
    let db_path = dir.path().join("qso.db");
    std::fs::write(
        &adif_path,
        "<EOH>\n<CALL:5>K1ABC <QSO_DATE:8>20260101 <TIME_ON:6>140000 <MODE:3>FT8 <FREQ:9>14.075200 <BAND:3>20M <EOR>\n",
    )
    .unwrap();
    // Seeded ADIF: prepared in-memory seed, replay source with no paths.
    let prepared = replay_history_db(&adif_path).await;
    assert!(prepared.is_ok());
    assert!(
        matches!(
            select_history_source(true, &prepared, db_path.clone(), adif_path.clone()),
            HistorySource::Replay { seed: Ok(_) }
        ),
        "replay with a prepared seed must not select the disk reader"
    );
    assert!(!db_path.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    // Missing ADIF: a valid empty seed, still replay, still no files.
    let empty_dir = tempfile::tempdir().unwrap();
    let missing = empty_dir.path().join("qsos.adi");
    let prepared = replay_history_db(&missing).await;
    assert!(prepared.is_ok());
    assert!(matches!(
        select_history_source(true, &prepared, missing.clone(), missing.clone()),
        HistorySource::Replay { seed: Ok(_) }
    ));
    assert_eq!(std::fs::read_dir(empty_dir.path()).unwrap().count(), 0);
    // Failed preparation (existence probe under a regular file): the
    // failure is retained and can never select the live disk reader.
    let blocker = empty_dir.path().join("blocker");
    std::fs::write(&blocker, b"regular file").unwrap();
    let broken = empty_dir.path().join("blocker").join("qsos.adi");
    let prepared = replay_history_db(&broken).await;
    assert!(prepared.is_err());
    assert!(matches!(
        select_history_source(true, &prepared, db_path.clone(), broken.clone()),
        HistorySource::Replay { seed: Err(_) }
    ));
    // Live startup keeps the disk paths.
    assert!(matches!(
        select_history_source(false, &prepared, db_path.clone(), adif_path.clone()),
        HistorySource::Live { .. }
    ));
}
