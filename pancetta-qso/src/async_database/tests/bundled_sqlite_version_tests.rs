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

/// sqlx-sqlite 0.8 always built unlock-notify; 0.9 builds it only with
/// sqlx's `sqlite-unlock-notify` feature. Without it, two pooled
/// connections on a shared-cache `:memory:` database (replay mode) fail with
/// "database table is locked" instead of waiting.
#[tokio::test]
async fn qso_index_sqlite_keeps_unlock_notify() {
    let db = QsoDatabase::new_in_memory().await.unwrap();

    let enabled: i64 =
        sqlx::query_scalar("SELECT sqlite_compileoption_used('ENABLE_UNLOCK_NOTIFY')")
            .fetch_one(&db.pool)
            .await
            .unwrap();

    assert_eq!(
        enabled, 1,
        "bundled SQLite lacks SQLITE_ENABLE_UNLOCK_NOTIFY"
    );
}
