//! Process-kill regression for the atomic index publication boundary.
use pancetta_qso::async_database::QsoDatabase;
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
const CHILD_ENV: &str = "PANCETTA_PAN282_REBUILD_DIR";
#[tokio::test]
#[ignore = "child process for a_kill_during_rebuild_leaves_the_old_or_the_new_index"]
async fn rebuild_child() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    let dir = Path::new(&dir);
    QsoDatabase::replay_from_adif(dir.join("qso.db"), dir.join("qsos.adi"))
        .await
        .unwrap()
        .close()
        .await;
}
fn log(n: usize) -> String {
    let mut s = String::from("<EOH>\n");
    for i in 0..n {
        let call = format!("K{i}ABC");
        s += &format!("<CALL:{}>{call} <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20M <EOR>\n",call.len());
    }
    s
}
fn child(dir: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["rebuild_child", "--exact", "--ignored", "--test-threads=1"])
        .env(CHILD_ENV, dir)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
#[test]
fn a_kill_during_rebuild_leaves_the_old_or_the_new_index() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("old.adi");
    std::fs::write(&source, log(100)).unwrap();
    rt.block_on(async {
        QsoDatabase::replay_from_adif(fixture.path().join("qso.db"), &source)
            .await
            .unwrap()
            .close()
            .await;
    });
    let new_log = log(10_000);
    let setup = || {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixture.path().join("qso.db"), dir.path().join("qso.db")).unwrap();
        std::fs::write(dir.path().join("qsos.adi"), &new_log).unwrap();
        dir
    };
    let calibration = setup();
    let start = Instant::now();
    assert!(child(calibration.path()).wait().unwrap().success());
    let elapsed = start.elapsed();
    for fraction in [0.05, 0.15, 0.3, 0.45, 0.6, 0.75, 0.9, 1.1] {
        let dir = setup();
        let mut process = child(dir.path());
        std::thread::sleep(Duration::from_secs_f64(elapsed.as_secs_f64() * fraction));
        let _ = process.kill();
        process.wait().unwrap();
        rt.block_on(async {
            let db = QsoDatabase::open(dir.path().join("qso.db")).await.unwrap();
            use sqlx::Connection;
            let mut conn = sqlx::SqliteConnection::connect_with(
                &sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("qso.db")),
            )
            .await
            .unwrap();
            let check: String = sqlx::query_scalar("PRAGMA integrity_check")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(check, "ok");
            conn.close().await.unwrap();
            let n = db.count_qsos().await.unwrap();
            assert!(n == 100 || n == 10_000, "partial index: {n}");
            db.close().await;
        });
    }
}
