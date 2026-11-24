//! PAN-394: the QSO index runs on a SQLite that RUSTSEC-2026-0459 no longer
//! affects. Cargo Deny checks the lockfile; this checks the library the pool
//! actually talks to.

use super::*;

/// RUSTSEC-2026-0459 is fixed in SQLite 3.54.0 (libsqlite3-sys >= 0.39).
const FIRST_PATCHED_SQLITE: (u32, u32, u32) = (3, 54, 0);

fn parse_version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.split('.').map(|p| p.parse::<u32>().expect("numeric"));
    (
        parts.next().expect("major"),
        parts.next().expect("minor"),
        parts.next().unwrap_or(0),
    )
}

#[tokio::test]
async fn qso_index_runs_on_sqlite_patched_for_rustsec_2026_0459() {
    let tmp = tempfile::tempdir().unwrap();
    let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();

    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    assert!(
        parse_version(&version) >= FIRST_PATCHED_SQLITE,
        "qso.db runs on SQLite {version}, below 3.54.0 (RUSTSEC-2026-0459)"
    );
}

/// sqlx-sqlite 0.8.6 always built SQLite with unlock-notify; sqlx 0.9 builds it
/// only with `sqlite-unlock-notify`. A `:memory:` index (`--replay`,
/// `new_in_memory`) uses a shared cache, so without it a pooled reader fails
/// with "database table is locked" while another connection holds an
/// uncommitted write, instead of waiting for that write to commit.
#[tokio::test]
async fn in_memory_index_reader_waits_for_a_shared_cache_writer() {
    use std::time::Duration;

    let db = QsoDatabase::new_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE lock_probe (n INTEGER)")
        .execute(&db.pool)
        .await
        .unwrap();

    // Open the reader's connection first, so connection setup is not part of
    // the race with the commit below.
    let mut reader_conn = db.pool.acquire().await.unwrap();
    let mut writer = db.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO lock_probe (n) VALUES (1)")
        .execute(&mut *writer)
        .await
        .unwrap();

    let reader = tokio::spawn(async move {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM lock_probe")
            .fetch_one(&mut *reader_conn)
            .await
    });

    // Let the reader reach the table lock before the writer commits.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !reader.is_finished(),
        "reader finished while the writer still held the table lock: {:?}",
        reader.await
    );
    writer.commit().await.unwrap();

    let count = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .expect("reader still blocked 10 s after the writer committed")
        .unwrap()
        .expect("reader failed on the shared-cache lock instead of waiting for the commit");
    assert_eq!(count, 1);
}
