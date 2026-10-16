//! PAN-127: `pancetta import <FILE> [--dry-run]` against the real binary.
//!
//! Each test runs the binary with `$HOME`/XDG pinned to a scratch directory
//! and an empty scratch cwd (the same env pinning as `config_cli.rs`), then
//! reads `~/.pancetta/qsos.adi` and `~/.pancetta/qso.db` back through
//! `pancetta_qso`. No `--headless`: that turns on console logging, which
//! would mix log lines into the command's output.

use assert_cmd::Command;
use pancetta_qso::adif::{AdifProcessor, AdifRecord};
use pancetta_qso::async_database::QsoDatabase;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

/// What pancetta's own writer stamps on every QSO it logs; an imported
/// contact must never carry it.
const PANCETTA_ATTRIBUTION: &str = "Using cqdx.io & Pancetta";

/// A scratch `$HOME` and an empty scratch working directory.
struct Scratch {
    home: TempDir,
    cwd: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.home.path().join(".pancetta")
    }

    fn adif(&self) -> PathBuf {
        self.data_dir().join("qsos.adi")
    }

    fn db(&self) -> PathBuf {
        self.data_dir().join("qso.db")
    }

    /// Write `bytes` to a file in the scratch home (outside `~/.pancetta`).
    fn write_file(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Put `adif_text` in place as `~/.pancetta/qsos.adi`, with a matching
    /// `qso.db` built from it when `with_index`.
    fn seed_logbook(&self, adif_text: &str, with_index: bool) {
        std::fs::create_dir_all(self.data_dir()).unwrap();
        std::fs::write(self.adif(), adif_text).unwrap();
        if with_index {
            build_index(&self.db(), &self.adif());
        }
    }

    /// The real binary, pinned to this scratch home and cwd.
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.env("HOME", self.home.path())
            // `dirs` consults these before `$HOME` on Linux/macOS.
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            // `dirs::home_dir()` uses the profile directory on Windows.
            .env("USERPROFILE", self.home.path())
            .env_remove("PANCETTA_CALLSIGN")
            .current_dir(self.cwd.path())
            .timeout(Duration::from_secs(60));
        cmd
    }

    fn import(&self, file: &Path, dry_run: bool) -> std::process::Output {
        let mut cmd = self.command();
        cmd.arg("import").arg(file);
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.output().unwrap()
    }

    /// `pancetta import FILE`, which must succeed.
    fn import_ok(&self, file: &Path) -> String {
        let out = self.import(file, false);
        assert!(
            out.status.success(),
            "stdout: {}\nstderr: {}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    /// Names of every backup in `~/.pancetta`.
    fn backups(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.data_dir()) else {
            return Vec::new();
        };
        entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bak"))
            .collect()
    }
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn build_index(db: &Path, adif: &Path) {
    block_on(async {
        QsoDatabase::replay_from_adif(db, adif)
            .await
            .unwrap()
            .close()
            .await
    });
}

/// Rows in `qso.db`, and the calls it has as worked on 20m.
fn read_index(db: &Path) -> (u64, Vec<String>) {
    assert!(db.exists(), "{} does not exist", db.display());
    block_on(async {
        let index = QsoDatabase::open(db).await.unwrap();
        let count = index.count_qsos().await.unwrap();
        let worked = index.get_worked_callsigns("20M").await;
        index.close().await;
        (count, worked)
    })
}

/// Every record in `~/.pancetta/qsos.adi`, through pancetta's own reader.
fn logbook_records(adif: &Path) -> Vec<AdifRecord> {
    let text = std::fs::read_to_string(adif).unwrap();
    AdifProcessor::new()
        .parse_string(&text)
        .unwrap_or_else(|e| panic!("{} does not parse: {e}\n{text}", adif.display()))
        .records
}

fn value<'a>(record: &'a AdifRecord, name: &str) -> Option<&'a str> {
    record.fields.get(name).map(|f| f.value.as_str())
}

fn calls(records: &[AdifRecord]) -> Vec<String> {
    records
        .iter()
        .map(|r| value(r, "CALL").unwrap_or_default().to_string())
        .collect()
}

fn find<'a>(records: &'a [AdifRecord], call: &str) -> &'a AdifRecord {
    records
        .iter()
        .find(|r| value(r, "CALL") == Some(call))
        .unwrap_or_else(|| panic!("no {call} in {:?}", calls(records)))
}

/// `<name:len>value ` with the byte length, as pancetta and most loggers write it.
fn field(name: &str, value: &str) -> String {
    format!("<{}:{}>{} ", name, value.len(), value)
}

/// One record as WSJT-X writes it: lowercase tags and band, one per line.
fn wsjtx_record(call: &str, grid: &str, time_on: &str, freq: &str) -> String {
    [
        field("call", call),
        field("gridsquare", grid),
        field("mode", "FT8"),
        field("rst_sent", "-10"),
        field("rst_rcvd", "-12"),
        field("qso_date", "20240101"),
        field("time_on", time_on),
        field("qso_date_off", "20240101"),
        field("time_off", time_on),
        field("band", "20m"),
        field("freq", freq),
        field("station_callsign", "K5ARH"),
        field("my_gridsquare", "EM10"),
    ]
    .concat()
        + "<eor>\n"
}

fn wsjtx_file(records: &[String]) -> String {
    format!("WSJT-X ADIF Export<eoh>\n{}", records.concat())
}

/// Three new 20m FT8 contacts, 15 minutes apart.
fn three_contacts() -> String {
    wsjtx_file(&[
        wsjtx_record("W1AW", "FN31", "120000", "14.075123"),
        wsjtx_record("K1ABC", "FN42", "121500", "14.076000"),
        wsjtx_record("N0XYZ", "EM28", "123000", "14.074500"),
    ])
}

/// A logbook as pancetta's own writer leaves it: its header, then one
/// record per completed QSO in upper case.
fn pancetta_logbook(records: &[String]) -> String {
    format!(
        "ADIF Export by pancetta\n\n{}{}\n<EOH>\n\n{}",
        field("ADIF_VER", "3.1.0"),
        field("PROGRAMID", "pancetta"),
        records.concat()
    )
}

fn pancetta_record(call: &str, date: &str, time_on: &str) -> String {
    [
        field("CALL", call),
        field("QSO_DATE", date),
        field("TIME_ON", time_on),
        field("MODE", "FT8"),
        field("BAND", "20M"),
        field("FREQ", "14.074000"),
        field("RST_SENT", "-07"),
        field("RST_RCVD", "-11"),
    ]
    .concat()
        + "<EOR>\n\n"
}

#[test]
fn import_adds_contacts_to_a_new_logbook() {
    let s = Scratch::new();
    let file = s.write_file("wsjtx_log.adi", three_contacts());

    let out = s.import_ok(&file);
    assert!(
        out.contains(&format!("Import file: {} (3 records)\n", file.display())),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "Logbook:     {} (new)\n",
            s.home.path().join(".pancetta/qsos.adi").display()
        )),
        "{out}"
    );
    assert!(out.contains("Backup:      none (new logbook)\n"), "{out}");
    assert!(out.contains("  Added:              3\n"), "{out}");
    assert!(
        out.contains(&format!(
            "To undo this import: stop Pancetta, then delete {} and {}.",
            s.adif().display(),
            s.db().display()
        )),
        "{out}"
    );

    let text = std::fs::read_to_string(s.adif()).unwrap();
    assert!(!text.contains(PANCETTA_ATTRIBUTION), "{text}");
    let records = logbook_records(&s.adif());
    assert_eq!(calls(&records), ["W1AW", "K1ABC", "N0XYZ"]);
    for (call, grid) in [("W1AW", "FN31"), ("K1ABC", "FN42"), ("N0XYZ", "EM28")] {
        let record = find(&records, call);
        assert_eq!(value(record, "GRIDSQUARE"), Some(grid));
        assert_eq!(value(record, "RST_SENT"), Some("-10"));
        assert_eq!(value(record, "RST_RCVD"), Some("-12"));
        assert_eq!(value(record, "BAND"), Some("20m"));
        assert_eq!(value(record, "QSL_SENT"), None);
    }

    let (count, worked) = read_index(&s.db());
    assert_eq!(count, 3);
    for call in ["W1AW", "K1ABC", "N0XYZ"] {
        assert!(worked.iter().any(|w| w == call), "{call} not in {worked:?}");
    }

    let export = s
        .command()
        .args(["export", "--output", "out.adi"])
        .output()
        .unwrap();
    assert!(export.status.success(), "stderr: {}", stderr(&export));
    assert!(
        stdout(&export).contains("Found 3 QSO(s) in database"),
        "{}",
        stdout(&export)
    );
}

#[test]
fn import_backs_up_the_logbook_first() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        true,
    );
    let original = std::fs::read(s.adif()).unwrap();
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(s.db()).unwrap().ino()
    };
    let file = s.write_file(
        "wsjtx_log.adi",
        wsjtx_file(&[
            wsjtx_record("W1AW", "FN31", "120000", "14.075123"),
            wsjtx_record("K1ABC", "FN42", "121500", "14.076000"),
        ]),
    );

    let out = s.import_ok(&file);
    let bak = s.data_dir().join("qsos.adi.bak");
    assert!(
        out.contains(&format!(
            "Logbook:     {} (1 contact)\nBackup:      {}\n\n",
            s.adif().display(),
            bak.display()
        )),
        "{out}"
    );
    assert!(out.contains("  Added:              2\n"), "{out}");
    assert!(
        out.contains(&format!(
            "To undo this import: stop Pancetta, copy {} over {}, then delete {}.",
            bak.display(),
            s.adif().display(),
            s.db().display()
        )),
        "{out}"
    );

    assert_eq!(std::fs::read(&bak).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bak).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "backup mode {mode:o} is not owner-only");
    }
    let after = std::fs::read(s.adif()).unwrap();
    assert!(after.starts_with(&original), "the logbook was rewritten");
    assert_eq!(
        calls(&logbook_records(&s.adif())),
        ["KA1AA", "W1AW", "K1ABC"]
    );

    let (count, _) = read_index(&s.db());
    assert_eq!(count, 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(s.db()).unwrap().ino(),
            inode,
            "the index was recreated instead of inserted into"
        );
    }
}

/// An index that was already behind the logbook before the import (startup
/// rebuilds it when `qsos.adi` is newer) must still look behind afterwards,
/// or the rows it was missing would never be rebuilt.
#[test]
fn import_keeps_a_stale_index_looking_stale() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        true,
    );
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .append(true)
        .open(s.db())
        .and_then(|f| f.set_modified(an_hour_ago))
        .unwrap();
    let file = s.write_file(
        "wsjtx_log.adi",
        wsjtx_file(&[wsjtx_record("W1AW", "FN31", "120000", "14.075123")]),
    );

    s.import_ok(&file);

    let modified = |p: PathBuf| std::fs::metadata(p).unwrap().modified().unwrap();
    assert!(
        modified(s.adif()) > modified(s.db()),
        "qso.db must stay older than qsos.adi so the next start rebuilds it"
    );
}

/// When indexing fails, the logbook must be left looking newer than any
/// `qso.db` the attempt left behind: otherwise the next start treats it as
/// current and never repairs it. Here the logbook's second record cannot be
/// converted, so the index build fails — before creating `qso.db` at all,
/// since the replay converts every record before it touches the index.
#[test]
fn a_failed_index_update_leaves_the_logbook_newer() {
    let s = Scratch::new();
    let logbook = format!(
        "{}{}",
        pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        "<CALL:4>K1AB <TIME_ON:6>090500 <MODE:3>FT8 <BAND:3>20M <EOR>\n\n"
    );
    s.seed_logbook(&logbook, false);
    let file = s.write_file(
        "wsjtx_log.adi",
        wsjtx_file(&[wsjtx_record("W1AW", "FN31", "120000", "14.075123")]),
    );

    let out = s.import(&file, false);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("could not be updated"),
        "stderr: {}",
        stderr(&out)
    );

    let modified = |p: PathBuf| std::fs::metadata(p).ok().map(|m| m.modified().unwrap());
    assert!(
        modified(s.adif()) > modified(s.db()),
        "qso.db must be missing or older than qsos.adi so the next start rebuilds it"
    );
}

/// An import marks the index for a rebuild before it writes, and clears the
/// mark only once every new row is indexed: an import stopped part-way
/// (here, by an index failure) leaves it for the next start. A mark left by
/// an earlier interrupted import survives a later import that succeeds.
#[test]
fn an_unfinished_index_update_stays_marked_for_rebuild() {
    let s = Scratch::new();
    let marker = QsoDatabase::rebuild_marker(&s.db());
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        true,
    );
    let first = s.write_file(
        "first.adi",
        wsjtx_file(&[wsjtx_record("W1AW", "FN31", "120000", "14.075123")]),
    );
    s.import_ok(&first);
    assert!(!marker.exists(), "a finished import clears its mark");

    std::fs::write(&marker, b"").unwrap();
    let second = s.write_file(
        "second.adi",
        wsjtx_file(&[wsjtx_record("K1ABC", "FN42", "121500", "14.076000")]),
    );
    s.import_ok(&second);
    assert!(
        marker.exists(),
        "an earlier import's mark is not ours to clear"
    );

    let t = Scratch::new();
    let logbook = format!(
        "{}{}",
        pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        "<CALL:4>K1AB <TIME_ON:6>090500 <MODE:3>FT8 <BAND:3>20M <EOR>\n\n"
    );
    t.seed_logbook(&logbook, false);
    let file = t.write_file("wsjtx_log.adi", three_contacts());
    let out = t.import(&file, false);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    assert!(
        QsoDatabase::rebuild_marker(&t.db()).exists(),
        "an index update that did not finish stays marked"
    );
}

#[test]
fn reimporting_the_same_file_adds_nothing() {
    let s = Scratch::new();
    let file = s.write_file("wsjtx_log.adi", three_contacts());
    s.import_ok(&file);
    let logbook = std::fs::read(s.adif()).unwrap();

    let out = s.import_ok(&file);
    assert!(out.contains("  Added:              0\n"), "{out}");
    assert!(
        out.contains("  Duplicates skipped: 3  (3 already in your logbook)\n"),
        "{out}"
    );
    assert!(
        out.ends_with("\nNothing to add; your logbook was not changed.\n"),
        "{out}"
    );
    assert!(!out.contains("Backup:"), "{out}");
    assert_eq!(std::fs::read(s.adif()).unwrap(), logbook);
    assert_eq!(s.backups(), Vec::<String>::new());
    assert_eq!(read_index(&s.db()).0, 3);
}

#[test]
fn contacts_pancetta_already_logged_are_skipped() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("W1AW", "20240101", "120000")]),
        true,
    );
    // The same QSO as another logger wrote it: lower case, the QSO start
    // ten minutes after pancetta's CQ start, minutes-only time.
    let file = s.write_file(
        "n1mm.adi",
        format!(
            "{}{}",
            [
                field("call", "w1aw"),
                field("qso_date", "20240101"),
                field("time_on", "1210"),
                field("mode", "FT8"),
                field("band", "20m"),
                field("freq", "14.074000"),
                "<eor>\n".to_string(),
            ]
            .concat(),
            wsjtx_record("K1ABC", "FN42", "121500", "14.076000")
        ),
    );

    let out = s.import_ok(&file);
    assert!(out.contains("  Added:              1\n"), "{out}");
    assert!(
        out.contains("  Duplicates skipped: 1  (1 already in your logbook)\n"),
        "{out}"
    );
    assert_eq!(calls(&logbook_records(&s.adif())), ["W1AW", "K1ABC"]);
    assert_eq!(read_index(&s.db()).0, 2);
}

#[test]
fn dry_run_changes_nothing() {
    // (a) An existing logbook and index.
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
        true,
    );
    let adif_before = std::fs::read(s.adif()).unwrap();
    let db_before = std::fs::read(s.db()).unwrap();
    let file = s.write_file("wsjtx_log.adi", three_contacts());

    let out = s.import(&file, true);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("\nDry run: nothing was changed.\n"), "{text}");
    assert!(text.contains("  Would add:          3\n"), "{text}");
    assert!(
        text.ends_with("\nRun again without --dry-run to add these 3 contacts.\n"),
        "{text}"
    );
    assert!(!text.contains("Backup:"), "{text}");
    assert_eq!(std::fs::read(s.adif()).unwrap(), adif_before);
    assert_eq!(std::fs::read(s.db()).unwrap(), db_before);
    assert_eq!(s.backups(), Vec::<String>::new());

    // (b) A fresh install: the preview creates neither file.
    let s = Scratch::new();
    let file = s.write_file("wsjtx_log.adi", three_contacts());
    let mut cmd = s.command();
    let out = cmd.arg("import").arg("-n").arg(&file).output().unwrap();
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("Logbook:     {} (new)\n", s.adif().display())),
        "{}",
        stdout(&out)
    );
    assert!(!s.adif().exists());
    assert!(!s.db().exists());
}

#[test]
fn unreadable_records_are_listed_and_the_rest_imported() {
    let s = Scratch::new();
    let no_date = [
        field("call", "K1ABC"),
        field("time_on", "121500"),
        field("mode", "FT8"),
        field("freq", "14.076000"),
        "<eor>\n".to_string(),
    ]
    .concat();
    let bad_length = format!(
        "<CALL:x>N0XYZ {}{}{}<eor>\n",
        field("qso_date", "20240101"),
        field("time_on", "123000"),
        field("mode", "FT8")
    );
    let file = s.write_file(
        "mixed.adi",
        wsjtx_file(&[
            wsjtx_record("W1AW", "FN31", "120000", "14.075123"),
            no_date,
            bad_length,
            wsjtx_record("KB2BBB", "FN20", "124500", "14.074800"),
        ]),
    );

    let out = s.import_ok(&file);
    assert!(out.contains("(4 records)"), "{out}");
    assert!(out.contains("  Added:              2\n"), "{out}");
    assert!(out.contains("  Could not read:     2\n"), "{out}");
    assert!(
        out.contains(concat!(
            "Records that could not be read:\n",
            "  record 2 (K1ABC): Missing required field: QSO_DATE\n",
            "  record 3: Invalid field format: CALL:x\n",
        )),
        "{out}"
    );
    assert_eq!(calls(&logbook_records(&s.adif())), ["W1AW", "KB2BBB"]);
    assert_eq!(read_index(&s.db()).0, 2);
}

#[test]
fn files_other_loggers_write_are_read() {
    let s = Scratch::new();
    let qso = |call: &str, time_on: &str| {
        [
            field("CALL", call),
            field("QSO_DATE", "20240102"),
            field("TIME_ON", time_on),
            field("MODE", "FT8"),
            field("FREQ", "14.074000"),
        ]
        .concat()
    };

    // A headerless file, one record per line.
    let headerless = s.write_file(
        "headerless.adi",
        format!(
            "{}<EOR>\n{}<EOR>\n",
            qso("W1AW", "100000"),
            qso("K1ABC", "101000")
        ),
    );
    // The header and both records on one line.
    let one_line = s.write_file(
        "one_line.adi",
        format!(
            "Exported by a logger <EOH>{}<EOR>{}<EOR>",
            qso("N0XYZ", "102000"),
            qso("KB2BBB", "103000")
        ),
    );
    // Latin-1, not UTF-8: "José" with é as the single byte 0xE9.
    let mut latin1 = format!("<EOH>{}<NAME:4>Jos", qso("EA1AAA", "104000")).into_bytes();
    latin1.extend_from_slice(b"\xe9 <EOR>\n");
    let latin1 = s.write_file("latin1.adi", latin1);
    // UTF-8 with lengths counted in characters, as WSJT-X-family writers do.
    let chars = s.write_file(
        "chars.adi",
        format!(
            "<EOH>{}<NAME:5>Müñoz <QTH:6>Zürich <EOR>\n",
            qso("EA2BBB", "105000")
        ),
    );

    for (file, added) in [(&headerless, 2), (&one_line, 2), (&latin1, 1), (&chars, 1)] {
        let out = s.import_ok(file);
        assert!(
            out.contains(&format!("  Added:              {added}\n")),
            "{}: {out}",
            file.display()
        );
        assert!(out.contains("  Could not read:     0\n"), "{out}");
    }

    let records = logbook_records(&s.adif());
    assert_eq!(
        calls(&records),
        ["W1AW", "K1ABC", "N0XYZ", "KB2BBB", "EA1AAA", "EA2BBB"]
    );
    assert_eq!(value(find(&records, "EA1AAA"), "NAME"), Some("José"));
    let ea2 = find(&records, "EA2BBB");
    assert_eq!(value(ea2, "NAME"), Some("Müñoz"));
    assert_eq!(value(ea2, "QTH"), Some("Zürich"));
    assert_eq!(read_index(&s.db()).0, 6);
}

#[test]
fn refuses_a_missing_or_non_adif_file() {
    let s = Scratch::new();

    let out = s.import(Path::new("missing.adi"), false);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("Error: Cannot read import file missing.adi"),
        "{}",
        stderr(&out)
    );

    let notes = s.write_file("notes.txt", "hello\n");
    let out = s.import(&notes, false);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!(
            "No ADIF records found in {} — is it an ADIF (.adi) file?",
            notes.display()
        )),
        "{}",
        stderr(&out)
    );

    assert!(!s.adif().exists());
    assert!(!s.db().exists());
}

#[test]
fn refuses_a_legacy_index_without_an_adif_log() {
    let s = Scratch::new();
    // A qso.db from before qsos.adi existed: built here from an ADIF file
    // that is not the logbook.
    let elsewhere = tempfile::tempdir().unwrap();
    let legacy_adif = elsewhere.path().join("legacy.adi");
    std::fs::write(
        &legacy_adif,
        pancetta_logbook(&[pancetta_record("KA1AA", "20231201", "090000")]),
    )
    .unwrap();
    std::fs::create_dir_all(s.data_dir()).unwrap();
    build_index(&s.db(), &legacy_adif);
    let db_before = std::fs::read(s.db()).unwrap();
    let file = s.write_file("wsjtx_log.adi", three_contacts());

    let out = s.import(&file, false);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!(
            "Pancetta has not yet moved your log from {} into {}. Start Pancetta once so it \
             can do that, then run this import again.",
            s.db().display(),
            s.adif().display()
        )),
        "{}",
        stderr(&out)
    );
    assert!(!s.adif().exists());
    assert_eq!(std::fs::read(s.db()).unwrap(), db_before);
}
