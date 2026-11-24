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
