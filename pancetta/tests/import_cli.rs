//! PAN-127: `pancetta import <FILE> [--dry-run]` against the real binary;
//! PAN-214: `pancetta import --undo <RECEIPT>`, and an import taking turns
//! with Pancetta's live logging.
//!
//! Each test runs the binary with `$HOME`/XDG pinned to a scratch directory
//! and an empty scratch cwd (the same env pinning as `config_cli.rs`), then
//! reads `~/.pancetta/qsos.adi` and `~/.pancetta/qso.db` back through
//! `pancetta_qso`. No `--headless`: that turns on console logging, which
//! would mix log lines into the command's output.

use assert_cmd::Command;
use pancetta_qso::adif::{AdifProcessor, AdifRecord};
use pancetta_qso::adif_import::ImportReceipt;
use pancetta_qso::adif_log_writer::AdifLogWriter;
use pancetta_qso::async_database::QsoDatabase;
use pancetta_qso::logbook_lock::LogbookLock;
use pancetta_qso::QsoMetadata;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};
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
        let mut cmd = Command::from_std(self.std_command());
        cmd.timeout(Duration::from_secs(60));
        cmd
    }

    /// [`Self::command`] as a `std::process::Command`, so a test can
    /// `spawn()` it and act while it runs (PAN-214).
    fn std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("pancetta"));
        cmd.env("HOME", self.home.path())
            // `dirs` consults these before `$HOME` on Linux/macOS.
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            // `dirs::home_dir()` uses the profile directory on Windows.
            .env("USERPROFILE", self.home.path())
            .env_remove("PANCETTA_CALLSIGN")
            .current_dir(self.cwd.path());
        cmd
    }

    fn imports_dir(&self) -> PathBuf {
        self.data_dir().join("imports")
    }

    /// `pancetta import FILE` started in the background, its stdout read
    /// line by line as it runs.
    fn spawn_import(&self, file: &Path) -> Running {
        let mut child = self
            .std_command()
            .arg("import")
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        });
        Running {
            child,
            lines,
            stdout: String::new(),
            stderr: Some(stderr),
        }
    }

    /// `pancetta import --undo RECEIPT`.
    fn undo(&self, receipt: &Path) -> std::process::Output {
        let mut cmd = self.command();
        cmd.arg("import").arg("--undo").arg(receipt);
        cmd.output().unwrap()
    }

    /// `pancetta import --undo RECEIPT`, which must succeed.
    fn undo_ok(&self, receipt: &Path) -> String {
        let out = self.undo(receipt);
        assert!(
            out.status.success(),
            "stdout: {}\nstderr: {}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
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

/// A WSJT-X export of new 20m FT8 contacts with `calls`, 15 minutes apart.
fn contacts(calls: &[&str]) -> String {
    let records: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(i, call)| {
            let time_on = format!("{:02}{:02}00", 12 + i / 4, (i % 4) * 15);
            wsjtx_record(call, "FN31", &time_on, "14.075000")
        })
        .collect();
    wsjtx_file(&records)
}

/// A QSO as Pancetta's live logging hands it to `AdifLogWriter::append`,
/// built through the reader so the test needs no `QsoMetadata` literal.
fn live_qso(call: &str) -> QsoMetadata {
    let processor = AdifProcessor::new();
    let text = format!(
        "<EOH>\n{}<EOR>\n",
        [
            field("CALL", call),
            field("QSO_DATE", "20261026"),
            field("TIME_ON", "101700"),
            field("MODE", "FT8"),
            field("BAND", "20m"),
            field("FREQ", "14.075500"),
            field("STATION_CALLSIGN", "K5ARH"),
        ]
        .concat()
    );
    let file = processor.parse_string(&text).unwrap();
    processor.adif_to_qso(&processor.record_to_qso(&file.records[0]).unwrap())
}

/// Log `qso` the way a running Pancetta does: its own `AdifLogWriter` on
/// the logbook.
fn log_live(adif: &Path, qso: &QsoMetadata) {
    block_on(async {
        let writer = AdifLogWriter::open(adif).await.unwrap();
        writer.append(qso).await.unwrap();
    });
}

/// The first line of an import that added contacts, after the receipt path.
const UNDO_PREFIX: &str = "To undo this import, run: pancetta import --undo ";

/// The line after it (PAN-214).
const UNDO_PROMISE: &str = "Undoing removes only what this import added; contacts Pancetta \
                            logs in the meantime stay in your logbook.";

/// The receipt `pancetta import` named in its undo line, without the double
/// quotes it adds around a path with a space in it.
fn undo_command(stdout: &str) -> PathBuf {
    let arg = stdout
        .lines()
        .find_map(|line| line.strip_prefix(UNDO_PREFIX))
        .unwrap_or_else(|| panic!("no undo line in:\n{stdout}"));
    let arg = arg
        .strip_prefix('"')
        .and_then(|a| a.strip_suffix('"'))
        .unwrap_or(arg);
    PathBuf::from(arg)
}

/// `stdout` ends an import that added contacts with the two designed undo
/// lines, naming an owner-only receipt in `~/.pancetta/imports/`. Returns
/// the receipt.
fn assert_undo_lines(s: &Scratch, stdout: &str) -> PathBuf {
    let receipt = undo_command(stdout);
    let arg = stdout
        .lines()
        .find_map(|line| line.strip_prefix(UNDO_PREFIX))
        .unwrap();
    assert!(
        stdout.ends_with(&format!("\n{UNDO_PREFIX}{arg}\n{UNDO_PROMISE}\n")),
        "{stdout}"
    );
    assert!(!stdout.contains("stop Pancetta"), "{stdout}");
    assert_eq!(
        receipt.parent(),
        Some(s.imports_dir().as_path()),
        "{stdout}"
    );
    let name = receipt.file_name().unwrap().to_string_lossy().into_owned();
    let stamp = name
        .strip_prefix("import-")
        .and_then(|n| n.strip_suffix(".json"))
        .unwrap_or_else(|| panic!("receipt name {name}"));
    let (date, time) = stamp.split_once('-').unwrap();
    let time = time.split('-').next().unwrap();
    assert!(
        date.len() == 8
            && time.len() == 6
            && (date.to_owned() + time).bytes().all(|b| b.is_ascii_digit()),
        "receipt name {name} is not import-YYYYMMDD-HHMMSS.json"
    );
    assert!(receipt.is_file(), "{} does not exist", receipt.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&receipt).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "receipt mode {mode:o} is not owner-only");
    }
    receipt
}

fn read_receipt(receipt: &Path) -> ImportReceipt {
    ImportReceipt::from_json(&std::fs::read(receipt).unwrap()).unwrap()
}

/// Every receipt in `~/.pancetta/imports/`.
fn receipts(s: &Scratch) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(s.imports_dir()) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// A `pancetta` child started by [`Scratch::spawn_import`].
struct Running {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
    stdout: String,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Running {
    /// Read stdout until a line starting with `prefix` arrives.
    fn wait_for_line(&mut self, prefix: &str) {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let found = line.starts_with(prefix);
                    self.stdout.push_str(&line);
                    self.stdout.push('\n');
                    if found {
                        return;
                    }
                }
                Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    let stderr = self.stderr.take().unwrap().join().unwrap();
                    panic!(
                        "no line starting {prefix:?}\nstdout: {}\nstderr: {stderr}",
                        self.stdout
                    );
                }
            }
        }
    }

    /// Wait for the child to exit, which it must do successfully; its whole
    /// stdout.
    fn finish_ok(mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("pancetta did not exit\nstdout: {}", self.stdout);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        for line in self.lines.iter() {
            self.stdout.push_str(&line);
            self.stdout.push('\n');
        }
        let stderr = self.stderr.take().unwrap().join().unwrap();
        assert!(
            status.success(),
            "stdout: {}\nstderr: {stderr}",
            self.stdout
        );
        self.stdout
    }
}

/// The logbook lock is held while `pancetta import FILE` and a live
/// `AdifLogWriter::append(W1AW)` both wait for it; then it is let go and
/// either may go first (PAN-214). Returns the import's stdout.
fn import_racing_a_live_append(s: &Scratch, file: &Path) -> String {
    let lock = LogbookLock::open(&s.adif()).unwrap();
    let guard = lock
        .try_lock()
        .unwrap()
        .expect("nothing else holds the logbook lock");
    let before = std::fs::read(s.adif()).unwrap();

    let mut import = s.spawn_import(file);
    import.wait_for_line("Waiting for Pancetta to finish writing to ");
    let adif = s.adif();
    let live = std::thread::spawn(move || log_live(&adif, &live_qso("W1AW")));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        std::fs::read(s.adif()).unwrap(),
        before,
        "a write went ahead while the logbook lock was held"
    );

    drop(guard);
    live.join().unwrap();
    import.finish_ok()
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
    assert_undo_lines(&s, &out);

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
    assert_undo_lines(&s, &out);

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
    // A later live write can make qso.db newer again, so the mtime alone
    // cannot carry the stale index to the next start (PR #451 review).
    assert!(
        QsoDatabase::rebuild_marker(&s.db()).exists(),
        "an index that was already behind stays marked for rebuild"
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

/// PAN-214 (PR #451 review, Codex): the file's other multi-byte values count
/// characters, so NOTES (which both units read cleanly) is read in
/// characters too and keeps its tag-shaped text.
#[test]
fn tag_shaped_text_in_a_character_counted_value_is_kept() {
    let s = Scratch::new();
    let file = s.write_file(
        "chars.adi",
        "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <NAME:5>Müñoz \
         <NOTES:9>€€€<X:1>z<MODE:3>FT8<EOR>\n\
         <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <QTH:6>Zürich \
         <MODE:3>FT8<EOR>\n",
    );

    let out = s.import_ok(&file);
    assert!(out.contains("  Added:              2\n"), "{out}");
    assert!(out.contains("  Could not read:     0\n"), "{out}");

    let records = logbook_records(&s.adif());
    assert_eq!(calls(&records), ["W1AW", "K1ABC"]);
    let w1aw = find(&records, "W1AW");
    assert_eq!(value(w1aw, "NOTES"), Some("€€€<X:1>z"));
    assert_eq!(value(w1aw, "NAME"), Some("Müñoz"));
    assert_eq!(value(w1aw, "MODE"), Some("FT8"));
    assert_eq!(value(w1aw, "X"), None);
    assert_eq!(value(find(&records, "K1ABC"), "QTH"), Some("Zürich"));
}

/// PAN-214 (PR #451 review, Codex): a length that runs past the end of the
/// file makes its own record unreadable and no longer swallows the next.
#[test]
fn an_overrunning_length_makes_only_its_record_unreadable() {
    let s = Scratch::new();
    let file = s.write_file(
        "overrun.adi",
        "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
         <COMMENT:999>x <EOR>\n\
         <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <MODE:3>FT8<EOR>\n",
    );

    let out = s.import_ok(&file);
    assert!(out.contains("  Added:              1\n"), "{out}");
    assert!(out.contains("  Could not read:     1\n"), "{out}");
    assert!(
        out.contains(concat!(
            "Records that could not be read:\n",
            "  record 1 (W1AW): Field length does not match its value: COMMENT:999\n",
        )),
        "{out}"
    );

    let records = logbook_records(&s.adif());
    assert_eq!(calls(&records), ["K1ABC"]);
    let k1abc = find(&records, "K1ABC");
    assert_eq!(value(k1abc, "TIME_ON"), Some("121500"));
    assert_eq!(value(k1abc, "COMMENT"), None);
}

/// PAN-214 (PR #451 review, Codex): `pancetta export` reads the index, and
/// writes an imported field's type indicator back as read.
#[test]
fn export_keeps_an_imported_type_indicator() {
    let s = Scratch::new();
    let file = s.write_file(
        "log4om.adi",
        format!(
            "<EOH>\n{}<APP_LOGGER_SCORE:2:N>42 <EOR>\n",
            [
                field("CALL", "W1AW"),
                field("QSO_DATE", "20240101"),
                field("TIME_ON", "120000"),
                field("MODE", "FT8"),
                field("BAND", "20m"),
                field("FREQ", "14.075123"),
            ]
            .concat()
        ),
    );
    s.import_ok(&file);

    let export = s
        .command()
        .args(["export", "--output", "out.adi"])
        .output()
        .unwrap();
    assert!(export.status.success(), "stderr: {}", stderr(&export));
    let text = std::fs::read_to_string(s.cwd.path().join("out.adi")).unwrap();
    assert!(text.contains("<APP_LOGGER_SCORE:2:N>42"), "{text}");
}

/// PAN-214: an import that finds the logbook lock held says so, waits
/// without writing, and goes ahead once the lock is free.
#[test]
fn import_waits_while_pancetta_writes_the_logbook() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let before = std::fs::read(s.adif()).unwrap();
    let file = s.write_file("three.adi", contacts(&["N0AAA", "N0BBB", "N0CCC"]));
    let lock = LogbookLock::open(&s.adif()).unwrap();
    let guard = lock
        .try_lock()
        .unwrap()
        .expect("nothing else holds the logbook lock");

    let mut import = s.spawn_import(&file);
    import.wait_for_line("Waiting for Pancetta to finish writing to ");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        std::fs::read(s.adif()).unwrap(),
        before,
        "the import wrote while the logbook lock was held"
    );
    assert_eq!(s.backups(), Vec::<String>::new());
    drop(guard);
    let out = import.finish_ok();

    let lines: Vec<&str> = out.lines().collect();
    let waiting = format!(
        "Waiting for Pancetta to finish writing to {}...",
        s.adif().display()
    );
    let at = lines
        .iter()
        .position(|line| *line == waiting)
        .unwrap_or_else(|| panic!("no {waiting:?} in:\n{out}"));
    assert!(lines[at - 1].starts_with("Logbook:     "), "{out}");
    assert!(lines[at + 1].starts_with("Backup:      "), "{out}");
    assert!(out.contains("  Added:              3\n"), "{out}");
    assert_eq!(
        calls(&logbook_records(&s.adif())),
        ["K9XYZ", "N0AAA", "N0BBB", "N0CCC"]
    );
}

/// PAN-214: a live append and an import that both waited for the lock take
/// turns, so no live QSO lands inside the imported block.
#[test]
fn an_import_never_interleaves_with_a_live_append() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("three.adi", contacts(&["N0AAA", "N0BBB", "N0CCC"]));

    let out = import_racing_a_live_append(&s, &file);
    assert!(out.contains("  Added:              3\n"), "{out}");

    let logged = calls(&logbook_records(&s.adif()));
    assert_eq!(logged[0], "K9XYZ", "{logged:?}");
    assert_eq!(
        logged.iter().filter(|c| *c == "W1AW").count(),
        1,
        "{logged:?}"
    );
    let first = logged.iter().position(|c| c == "N0AAA").unwrap();
    assert_eq!(
        logged[first..first + 3],
        ["N0AAA", "N0BBB", "N0CCC"],
        "the imported records are not one block: {logged:?}"
    );
    assert_eq!(logged.len(), 5, "{logged:?}");
}

/// PAN-214, the ticket's scenario: "Undoing an import keeps a contact logged
/// by a running Pancetta during the import".
#[test]
fn undo_removes_the_import_and_keeps_a_contact_logged_since() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let seeded = std::fs::read(s.adif()).unwrap();
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));
    let receipt = assert_undo_lines(&s, &s.import_ok(&file));
    let imported = std::fs::read(s.adif()).unwrap();
    log_live(&s.adif(), &live_qso("W1AW"));
    let before_undo = std::fs::read(s.adif()).unwrap();
    let w1aw = &before_undo[imported.len()..];
    assert!(!w1aw.is_empty());

    let out = s.undo_ok(&receipt);
    assert_eq!(
        calls(&logbook_records(&s.adif())),
        ["K9XYZ", "W1AW"],
        "{out}"
    );
    assert_eq!(
        std::fs::read(s.adif()).unwrap(),
        [&seeded[..], w1aw].concat(),
        "undo must leave the logbook as it was, plus W1AW"
    );

    let bak1 = s.data_dir().join("qsos.adi.bak.1");
    assert!(out.contains("lotw.adi (2 contacts, added "), "{out}");
    assert!(
        out.contains(&format!(
            "\nLogbook:     {}\nBackup:      {}\n\n  Removed:            2\n  \
             Logged since, kept: 1\n\n",
            s.adif().display(),
            bak1.display()
        )),
        "{out}"
    );
    assert!(
        out.ends_with(
            "\nPancetta drops the removed contacts from its search index (which `pancetta \
             export` reads) and its worked-station history the next time it starts.\n"
        ),
        "{out}"
    );
    assert!(!out.contains("stop Pancetta"), "{out}");
    assert_eq!(std::fs::read(&bak1).unwrap(), before_undo);
    assert!(
        QsoDatabase::rebuild_marker(&s.db()).exists(),
        "the index still holds the removed contacts, so it is marked for rebuild"
    );
    let receipt = read_receipt(&receipt);
    assert!(receipt.undone_at.is_some(), "{receipt:?}");
}

/// PAN-214: an undo run while another import is still indexing (here into a
/// new index, so `qso.db` is not there yet) writes its own name into that
/// import's rebuild marker, so the import does not clear the undo's rebuild
/// request when it finishes.
#[test]
fn undo_takes_over_the_rebuild_marker_of_an_import_still_indexing() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));
    let receipt = assert_undo_lines(&s, &s.import_ok(&file));
    std::fs::remove_file(s.db()).unwrap();
    let marker = QsoDatabase::rebuild_marker(&s.db());
    std::fs::write(&marker, "import /elsewhere/import-x.json\n").unwrap();

    s.undo_ok(&receipt);

    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("undo {}\n", receipt.display())
    );
}

/// PAN-214: undo also keeps a live QSO that was waiting on the lock with
/// the import, whichever of the two wrote first.
#[test]
fn undo_keeps_a_contact_logged_while_the_import_waited() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("three.adi", contacts(&["N0AAA", "N0BBB", "N0CCC"]));
    let receipt = undo_command(&import_racing_a_live_append(&s, &file));

    let out = s.undo_ok(&receipt);
    assert!(out.contains("  Removed:            3\n"), "{out}");
    assert_eq!(calls(&logbook_records(&s.adif())), ["K9XYZ", "W1AW"]);
}

/// PAN-214: an import that created the logbook is undone down to the header
/// it wrote, keeping what was logged since.
#[test]
fn undo_of_an_import_into_a_new_logbook_leaves_only_the_header() {
    let s = Scratch::new();
    let file = s.write_file("three.adi", contacts(&["N0AAA", "N0BBB", "N0CCC"]));
    let receipt = assert_undo_lines(&s, &s.import_ok(&file));
    let offset = usize::try_from(read_receipt(&receipt).offset).unwrap();
    let imported = std::fs::read(s.adif()).unwrap();
    let header = &imported[..offset];
    assert!(
        String::from_utf8_lossy(header).contains("<EOH>"),
        "{}",
        String::from_utf8_lossy(header)
    );
    log_live(&s.adif(), &live_qso("W1AW"));
    let w1aw = std::fs::read(s.adif()).unwrap()[imported.len()..].to_vec();

    let out = s.undo_ok(&receipt);
    assert!(out.contains("  Removed:            3\n"), "{out}");
    assert!(out.contains("  Logged since, kept: 1\n"), "{out}");
    assert_eq!(
        std::fs::read(s.adif()).unwrap(),
        [header, &w1aw[..]].concat()
    );
    assert_eq!(calls(&logbook_records(&s.adif())), ["W1AW"]);
}

#[test]
fn undoing_twice_is_refused() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));
    let receipt = undo_command(&s.import_ok(&file));
    s.undo_ok(&receipt);
    let logbook = std::fs::read(s.adif()).unwrap();
    let backups = s.backups().len();

    let out = s.undo(&receipt);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("Error: This import was already undone at "),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains(" UTC; your logbook was not changed."),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(s.adif()).unwrap(), logbook);
    assert_eq!(s.backups().len(), backups);
}

/// PAN-214: undo removes the imported bytes only when they are still there
/// exactly as added; otherwise it refuses and names the pre-import backup.
#[test]
fn undo_refuses_a_logbook_edited_inside_the_import() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));
    let receipt = undo_command(&s.import_ok(&file));
    let edited = std::fs::read_to_string(s.adif())
        .unwrap()
        .replacen("N0BBB", "N0BBC", 1);
    std::fs::write(s.adif(), &edited).unwrap();

    let out = s.undo(&receipt);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!(
            "Error: Your logbook no longer holds the contacts this import added exactly as \
             they were added, so they cannot be removed automatically; nothing was changed. \
             Your logbook as it was before the import is {}.",
            s.data_dir().join("qsos.adi.bak").display()
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(s.adif()).unwrap(), edited);
    assert_eq!(s.backups(), ["qsos.adi.bak"]);
    assert_eq!(read_receipt(&receipt).undone_at, None);
}

#[test]
fn undo_refuses_a_receipt_for_another_logbook() {
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));
    let receipt = undo_command(&s.import_ok(&file));
    let other = tempfile::tempdir().unwrap();
    let other_logbook = other.path().join(".pancetta").join("qsos.adi");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    json["logbook"] = serde_json::Value::String(other_logbook.to_string_lossy().into_owned());
    let foreign = s.write_file("other-receipt.json", json.to_string());
    let logbook = std::fs::read(s.adif()).unwrap();

    let out = s.undo(&foreign);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!(
            "Error: This receipt belongs to the logbook {}, not {}; nothing was changed.",
            other_logbook.display(),
            s.adif().display()
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(s.adif()).unwrap(), logbook);
    assert_eq!(s.backups(), ["qsos.adi.bak"]);
}

/// PAN-214: each receipt finds its own block, at its offset or moved up by
/// an earlier undo, so two imports undo in either order.
#[test]
fn imports_can_be_undone_in_either_order() {
    for first_undone in ["A", "B"] {
        let s = Scratch::new();
        s.seed_logbook(
            &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
            true,
        );
        let original = std::fs::read(s.adif()).unwrap();
        let a = s.write_file("a.adi", contacts(&["N0AAA", "N0BBB"]));
        let b = s.write_file("b.adi", contacts(&["N0CCC", "N0DDD"]));
        let receipt_a = undo_command(&s.import_ok(&a));
        let receipt_b = undo_command(&s.import_ok(&b));
        assert_ne!(receipt_a, receipt_b);
        assert_eq!(
            calls(&logbook_records(&s.adif())),
            ["K9XYZ", "N0AAA", "N0BBB", "N0CCC", "N0DDD"]
        );

        let (first, second, left) = if first_undone == "A" {
            (&receipt_a, &receipt_b, ["K9XYZ", "N0CCC", "N0DDD"])
        } else {
            (&receipt_b, &receipt_a, ["K9XYZ", "N0AAA", "N0BBB"])
        };
        s.undo_ok(first);
        assert_eq!(
            calls(&logbook_records(&s.adif())),
            left,
            "after undoing {first_undone} first"
        );
        s.undo_ok(second);
        assert_eq!(
            std::fs::read(s.adif()).unwrap(),
            original,
            "undoing {first_undone} first, then the other"
        );
    }
}

/// PAN-214: the receipt is written before the append, and removed when the
/// append fails, so no receipt names contacts that were never added.
#[cfg(unix)]
#[test]
fn a_failed_append_leaves_no_receipt() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.seed_logbook(
        &pancetta_logbook(&[pancetta_record("K9XYZ", "20231201", "090000")]),
        true,
    );
    std::fs::set_permissions(s.adif(), std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .append(true)
        .open(s.adif())
        .is_ok()
    {
        eprintln!("skipped: this user can write a read-only file (running as root?)");
        return;
    }
    let before = std::fs::read(s.adif()).unwrap();
    let file = s.write_file("lotw.adi", contacts(&["N0AAA", "N0BBB"]));

    let out = s.import(&file, false);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!("Failed to add contacts to {}", s.adif().display())),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(s.adif()).unwrap(), before);
    assert_eq!(receipts(&s), Vec::<String>::new());
}

#[test]
fn import_and_undo_flags_conflict() {
    let s = Scratch::new();
    let file = s.write_file("x.adi", three_contacts());
    let receipt = s.write_file("r.json", "{}");
    let run = |args: &[&std::ffi::OsStr]| {
        let out = s.command().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
        stderr(&out)
    };

    let err = run(&["import".as_ref()]);
    assert!(
        err.contains("error: the following required arguments were not provided:\n  <FILE>\n"),
        "{err}"
    );
    assert!(err.contains("Usage: pancetta import <FILE>"), "{err}");

    let err = run(&[
        "import".as_ref(),
        file.as_os_str(),
        "--undo".as_ref(),
        receipt.as_os_str(),
    ]);
    assert!(
        err.contains("error: the argument '[FILE]' cannot be used with '--undo <RECEIPT>'"),
        "{err}"
    );

    let err = run(&[
        "import".as_ref(),
        "--undo".as_ref(),
        receipt.as_os_str(),
        "--dry-run".as_ref(),
    ]);
    assert!(
        err.contains("error: the argument '--undo <RECEIPT>' cannot be used with '--dry-run'"),
        "{err}"
    );

    assert!(!s.adif().exists());
    assert_eq!(receipts(&s), Vec::<String>::new());
}
