//! Confirmation maintenance runs against the durable logbook, without station startup.
use pancetta_qso::{adif::AdifProcessor, async_database::QsoDatabase, logbook_lock::LogbookLock};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::Stdio,
};
use tempfile::TempDir;

const CONTACT: &str = "<CALL:6>DL1ABC <QSO_DATE:8>20260101 <TIME_ON:6>120000 <BAND:3>20M <MODE:3>FT8 <FREQ:6>14.074 <EOR>\n";
const QSL: &str = "<LOTW_QSL_RCVD:1>Y <LOTW_QSLRDATE:8>20260201 ";
struct Scratch {
    home: TempDir,
    cwd: TempDir,
}
impl Scratch {
    fn new(log: &str) -> Self {
        let s = Self {
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(s.data()).unwrap();
        std::fs::write(s.adif(), log).unwrap();
        s
    }
    fn data(&self) -> PathBuf {
        self.home.path().join(".pancetta")
    }
    fn adif(&self) -> PathBuf {
        self.data().join("qsos.adi")
    }
    fn db(&self) -> PathBuf {
        self.data().join("qso.db")
    }
    fn report(&self, text: &str) -> PathBuf {
        let p = self.home.path().join("report.adi");
        std::fs::write(&p, text).unwrap();
        p
    }
    fn command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
        c.env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env_remove("PANCETTA_CALLSIGN")
            .current_dir(self.cwd.path());
        c
    }
    fn import(&self, text: &str) -> std::process::Output {
        self.command()
            .arg("import-confirmations")
            .arg(self.report(text))
            .output()
            .unwrap()
    }
}
fn report() -> String {
    CONTACT.replace("<EOR>", &format!("{QSL}<EOR>"))
}
fn success(out: &std::process::Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout.clone()).unwrap()
}
#[tokio::test]
async fn lotw_report_confirms_existing_qso() {
    let s = Scratch::new(CONTACT);
    assert!(success(&s.import(&report())).contains("1 updated QSO"));
    let bytes = std::fs::read(s.adif()).unwrap();
    let parsed = AdifProcessor::new().parse_bytes(&bytes);
    assert_eq!(parsed.records.len(), 1);
    assert_eq!(
        parsed.records[0].as_ref().unwrap().fields["LOTW_QSL_RCVD"].value,
        "Y"
    );
    assert_eq!(
        parsed.records[0].as_ref().unwrap().fields["LOTW_QSLRDATE"].value,
        "20260201"
    );
    let db = QsoDatabase::replay_from_adif(":memory:", s.adif())
        .await
        .unwrap();
    let history = db.worked_history().await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].confirmed);
}
#[test]
fn repeat_import_is_a_noop() {
    let s = Scratch::new(CONTACT);
    success(&s.import(&report()));
    let before = std::fs::read(s.adif()).unwrap();
    std::fs::remove_file(QsoDatabase::rebuild_marker(&s.db())).unwrap();
    let out = success(&s.import(&report()));
    assert!(out.contains("1 unchanged"));
    assert!(out.contains("No logbook changes."));
    assert_eq!(std::fs::read(s.adif()).unwrap(), before);
    assert!(!s.data().join("qsos.adi.bak.1").exists());
    assert!(!QsoDatabase::rebuild_marker(&s.db()).exists());
}
#[test]
fn unrelated_bytes_and_fields_survive() {
    let log = format!(
        "Comment\n<ADIF_VER:5>3.1.4 <EOH>\n{}{}",
        CONTACT.replace("<EOR>", "<COMMENT:5:S><EOR> <EOR>"),
        CONTACT.replace("DL1ABC", "JA1ABC")
    );
    let s = Scratch::new(&log);
    success(&s.import(&report()));
    let after = std::fs::read_to_string(s.adif()).unwrap();
    assert!(after.starts_with("Comment\n<ADIF_VER:5>3.1.4 <EOH>\n"));
    assert!(after.contains("<COMMENT:5:S><EOR>"));
    assert!(after.ends_with(&CONTACT.replace("DL1ABC", "JA1ABC")));
    assert_eq!(
        std::fs::read_to_string(s.data().join("qsos.adi.bak")).unwrap(),
        log
    );
}
#[test]
fn unsafe_logbook_and_backup_failure_change_nothing() {
    for log in ["<CALL:99>broken", CONTACT] {
        let s = Scratch::new(log);
        if log == CONTACT {
            for i in 0..100 {
                let suffix = if i == 0 {
                    String::new()
                } else {
                    format!(".{i}")
                };
                std::fs::write(s.data().join(format!("qsos.adi.bak{suffix}")), "occupied").unwrap();
            }
        }
        let out = s.import(&report());
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(std::fs::read_to_string(s.adif()).unwrap(), log);
        assert!(!QsoDatabase::rebuild_marker(&s.db()).exists());
    }
}
#[test]
fn confirmation_import_waits_for_lock_and_keeps_live_append() {
    let s = Scratch::new(CONTACT);
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(s.adif())
        .unwrap();
    let lock = LogbookLock::open(&s.adif()).unwrap();
    let held = lock.try_lock().unwrap().unwrap();
    let mut child = s
        .command()
        .arg("import-confirmations")
        .arg(s.report(&report()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "Waiting for the logbook lock...");
    assert!(child.try_wait().unwrap().is_none());
    let other = CONTACT.replace("DL1ABC", "JA1ABC");
    append.write_all(other.as_bytes()).unwrap();
    append.sync_all().unwrap();
    drop(held);
    let output = child.wait_with_output().unwrap();
    success(&output);
    append.write_all(other.as_bytes()).unwrap();
    append.sync_all().unwrap();
    let parsed = AdifProcessor::new().parse_bytes(&std::fs::read(s.adif()).unwrap());
    assert_eq!(parsed.records.len(), 3);
    assert_eq!(
        parsed.records[0].as_ref().unwrap().fields["LOTW_QSL_RCVD"].value,
        "Y"
    );
}
#[test]
fn empty_missing_and_invalid_inputs() {
    let s = Scratch::new(CONTACT);
    assert!(success(&s.import("")).contains("0 records, 0 updated QSO"));
    assert!(!s.data().join("qsos.adi.bak").exists());
    let out = s
        .command()
        .args(["import-confirmations", "missing.adi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap().trim(),
        "Error: Cannot read confirmation report missing.adi."
    );
    let out = s.import("<CALL:99>broken");
    assert!(success(&out).contains("1 invalid"));
    std::fs::remove_file(s.adif()).unwrap();
    let out = s.import(&report());
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("import your contacts first"));
    assert_eq!(
        s.command()
            .arg("import-confirmations")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}
#[test]
fn confirmation_command_never_starts_station() {
    let s = Scratch::new(CONTACT);
    let invalid_config = s.home.path().join("bad.toml");
    std::fs::write(&invalid_config, "this is invalid").unwrap();
    let out = s
        .command()
        .arg("--config")
        .arg(invalid_config)
        .args(["--headless", "import-confirmations"])
        .arg(s.report(&report()))
        .output()
        .unwrap();
    let stdout = success(&out);
    assert!(!stdout.contains("Starting Pancetta"));
    assert!(out.stderr.is_empty());
    assert!(!s.data().join("logs").exists());
    assert!(!s.db().exists());
    assert!(!s.data().join("imports").exists());
}
#[test]
fn command_reports_every_record_category() {
    let s = Scratch::new(&format!(
        "{CONTACT}{}{}",
        CONTACT.replace("DL1ABC", "JA1ABC"),
        CONTACT.replace("DL1ABC", "JA1ABC")
    ));
    let input = format!(
        "{}{}{}{}{}",
        report(),
        report(),
        report().replace("DL1ABC", "JA1ABC"),
        report().replace("DL1ABC", "VK1ABC"),
        CONTACT
    );
    let out = s.import(&input);
    let stdout = success(&out);
    assert!(
        stdout.contains(
            "5 records, 1 updated QSO, 1 unchanged, 1 unmatched, 1 ambiguous, 1 invalid."
        ),
        "{stdout}"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        vec![
            "Record 3: multiple matching QSOs; skipped.",
            "Record 4: no matching QSO; skipped.",
            "Record 5: not a positive LoTW confirmation; skipped."
        ]
    );
}

#[tokio::test]
async fn restart_rebuilds_confirmation_index() {
    use pancetta_lib::{
        coordinator::prepare_local_qso_index, priority_evaluator::CachedStationLookup,
    };
    use pancetta_qso::priority::WorkedStationLookup;
    for existing in [false, true] {
        let s = Scratch::new(CONTACT);
        if existing {
            let stale = QsoDatabase::replay_from_adif(s.db(), s.adif())
                .await
                .unwrap();
            assert!(!stale.worked_history().await.unwrap()[0].confirmed);
            stale.close().await;
        }
        // A marker from another maintenance operation must survive the command.
        std::fs::write(QsoDatabase::rebuild_marker(&s.db()), "previous request").unwrap();
        success(&s.import(&report()));
        assert!(QsoDatabase::rebuild_marker(&s.db()).exists());
        if existing {
            let stale = QsoDatabase::open(s.db()).await.unwrap();
            assert!(
                !stale.worked_history().await.unwrap()[0].confirmed,
                "command must leave the running index alone"
            );
            stale.close().await;
        } else {
            assert!(!s.db().exists());
        }
        let db = match prepare_local_qso_index(&s.adif(), &s.db()).await {
            pancetta_lib::coordinator::LocalIndex::Ready(db) => db,
            _ => panic!("confirmation import must rebuild a ready index"),
        };
        let history = db.worked_history().await.unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].confirmed);
        let lookup = CachedStationLookup::new();
        lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
        lookup.seed_worked_history(&db).await.unwrap();
        assert!(!lookup.is_dxcc_needed_on_band("DL1ABC", 14_074_000.0));
        assert!(!lookup.is_atno("DL1ABC"));
        let export = s.home.path().join("export.adi");
        db.export_to_adif(&export).await.unwrap();
        let bytes = std::fs::read(export).unwrap();
        let parsed = AdifProcessor::new().parse_bytes(&bytes);
        assert_eq!(
            parsed.records[0].as_ref().unwrap().fields["LOTW_QSLRDATE"].value,
            "20260201"
        );
        assert!(!QsoDatabase::rebuild_marker(&s.db()).exists());
        db.close().await;
    }
}

#[test]
fn ordinary_import_undo_refuses_confirmation_changes() {
    let s = Scratch::new("");
    let out = s
        .command()
        .arg("import")
        .arg(s.report(CONTACT))
        .output()
        .unwrap();
    success(&out);
    let receipt = std::fs::read_dir(s.data().join("imports"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    success(&s.import(&report()));
    let before = std::fs::read(s.adif()).unwrap();
    let out = s
        .command()
        .args(["import", "--undo"])
        .arg(receipt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read(s.adif()).unwrap(), before);
}

// A file-size limit makes the rewrite fail after its first partial write while
// leaving the smaller, already-synced backup and marker writable.
#[cfg(target_os = "linux")]
#[test]
fn partial_write_failure_names_recovery_copy_and_retains_marker() {
    let padding = " ".repeat(500 - CONTACT.len());
    let log = format!("{padding}{CONTACT}");
    let s = Scratch::new(&log);
    let normal = s.command();
    let mut limited = std::process::Command::new("/bin/sh");
    for (key, value) in normal.get_envs() {
        if let Some(value) = value {
            limited.env(key, value);
        } else {
            limited.env_remove(key);
        }
    }
    limited
        .current_dir(s.cwd.path())
        .args([
            "-c",
            "trap '' XFSZ; ulimit -f 1; exec \"$@\"",
            "confirmation-test",
        ])
        .arg(normal.get_program())
        .arg("import-confirmations")
        .arg(s.report(&report()));
    let out = limited.output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("Recovery copy:"), "{stderr}");
    assert!(
        stderr.contains(s.data().join("qsos.adi.bak").to_str().unwrap()),
        "{stderr}"
    );
    assert!(QsoDatabase::rebuild_marker(&s.db()).exists());
    assert_eq!(
        std::fs::read_to_string(s.data().join("qsos.adi.bak")).unwrap(),
        log
    );
}

#[test]
fn a_busy_logbook_times_out_without_mutation() {
    let s = Scratch::new(CONTACT);
    let lock = LogbookLock::open(&s.adif()).unwrap();
    let _held = lock.try_lock().unwrap().unwrap();
    let out = s.import(&report());
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        vec![
            "Waiting for the logbook lock...",
            "Error: Logbook is busy; nothing was changed."
        ]
    );
    assert_eq!(std::fs::read_to_string(s.adif()).unwrap(), CONTACT);
    assert!(!s.data().join("qsos.adi.bak").exists());
    assert!(!QsoDatabase::rebuild_marker(&s.db()).exists());
}
