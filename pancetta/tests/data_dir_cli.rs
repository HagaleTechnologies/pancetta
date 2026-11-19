//! PAN-376 scenario 1: `pancetta --data-dir DIR` keeps config, logbook, logs,
//! station keys and the upload outbox under `DIR`, and nothing under
//! `~/.pancetta`.
//!
//! Each test runs the real binary with `$HOME`/XDG pinned to a scratch home,
//! an empty scratch working directory and a third scratch directory holding
//! the data directory (the same env pinning as `cli_surface.rs`).

use assert_cmd::Command;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DEMO_STATION: &str = "[station]\ncallsign = \"K1ABC\"\ngrid_square = \"FN42\"\n";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Also reap on assertion failure or a startup timeout.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A scratch `$HOME`, an empty scratch cwd and a scratch parent for the data
/// directory.
struct Scratch {
    home: TempDir,
    cwd: TempDir,
    data: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
        }
    }

    /// The `--data-dir` value: a directory that does not exist yet.
    fn data_dir(&self) -> PathBuf {
        self.data.path().join("pancetta-40m")
    }

    /// `~/.pancetta` under the scratch home.
    fn home_pancetta(&self) -> PathBuf {
        self.home.path().join(".pancetta")
    }

    fn assert_home_pancetta_untouched(&self) {
        assert!(
            !self.home_pancetta().exists(),
            "{} was created",
            self.home_pancetta().display()
        );
    }

    /// The real binary, pinned to this scratch home and cwd.
    fn std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PANCETTA_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env_remove("RUST_LOG")
            .current_dir(self.cwd.path());
        cmd
    }

    fn command(&self) -> Command {
        let mut cmd = Command::from_std(self.std_command());
        cmd.timeout(Duration::from_secs(60));
        cmd
    }

    /// `pancetta --data-dir <data_dir> ARGS`.
    fn with_data_dir(&self, args: &[&str]) -> std::process::Output {
        self.command()
            .arg("--data-dir")
            .arg(self.data_dir())
            .args(args)
            .output()
            .unwrap()
    }

    fn write_data_config(&self, body: &str) -> PathBuf {
        let dir = self.data_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        fs::write(&path, body).unwrap();
        path
    }

    /// `pancetta --data-dir <data_dir> --headless --no-audio`, its console
    /// output in a regular file (a file cannot fill and stall the child).
    fn spawn_station(&self) -> Station {
        let output_path = self.data.path().join("station-output.log");
        let output = File::create(&output_path).unwrap();
        let mut cmd = self.std_command();
        cmd.env("RUST_LOG", "info")
            .arg("--data-dir")
            .arg(self.data_dir())
            .args(["--headless", "--no-audio"])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        Station {
            child: ChildGuard(cmd.spawn().unwrap()),
            output: output_path,
        }
    }
}

struct Station {
    child: ChildGuard,
    output: PathBuf,
}

impl Station {
    fn console(&self) -> String {
        fs::read_to_string(&self.output).unwrap_or_default()
    }

    /// Poll until `done` holds, failing if the station exits first or
    /// `timeout` passes.
    fn wait_until(&mut self, what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if done() {
                return;
            }
            if let Some(status) = self.child.0.try_wait().unwrap() {
                panic!(
                    "station exited ({status}) before {what}:\n{}",
                    self.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                self.console()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_success(o: &std::process::Output) {
    assert!(
        o.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        o.status,
        stdout(o),
        stderr(o)
    );
}

/// Every line of every daily log file in `<dir>/logs`.
fn log_text(dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(dir.join("logs")) else {
        return String::new();
    };
    let mut text = String::new();
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("pancetta.log") {
            text.push_str(&fs::read_to_string(&path).unwrap_or_default());
        }
    }
    text
}

/// Names of the daily log files in `<dir>/logs`.
fn log_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir.join("logs")) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("pancetta.log"))
        .collect()
}

/// `Config file: …` lines of a `config --validate` report.
fn config_file_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with("Config file:"))
        .map(str::to_owned)
        .collect()
}

/// The directories a "No config file found" report lists as searched.
fn searched_dirs(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.starts_with("No config file found"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .map(|l| l.trim().to_owned())
        .collect()
}

/// Subcommand names as `pancetta --help` lists them.
fn subcommands(scratch: &Scratch) -> Vec<String> {
    let out = scratch.command().arg("--help").output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    help.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

/// `<name:len>value ` with the byte length.
fn field(name: &str, value: &str) -> String {
    format!("<{}:{}>{} ", name, value.len(), value)
}

/// One 20m FT8 contact as WSJT-X exports it.
fn wsjtx_file(call: &str) -> String {
    let record = [
        field("call", call),
        field("gridsquare", "FN31"),
        field("mode", "FT8"),
        field("rst_sent", "-10"),
        field("rst_rcvd", "-12"),
        field("qso_date", "20240101"),
        field("time_on", "120000"),
        field("qso_date_off", "20240101"),
        field("time_off", "120000"),
        field("band", "20m"),
        field("freq", "14.075123"),
        field("station_callsign", "K5ARH"),
        field("my_gridsquare", "EM10"),
    ]
    .concat();
    format!("WSJT-X ADIF Export<eoh>\n{record}<eor>\n")
}

/// A TOML literal string (single quotes, no escapes) for a path.
fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[test]
fn every_help_screen_offers_data_dir() {
    let scratch = Scratch::new();
    let names = subcommands(&scratch);
    assert!(names.iter().any(|n| n == "import"), "{names:?}");
    let mut screens = vec![vec!["--help".to_owned()]];
    for name in &names {
        screens.push(vec![name.clone(), "--help".to_owned()]);
    }
    for args in screens {
        let out = scratch.command().args(&args).output().unwrap();
        assert_success(&out);
        let help = stdout(&out);
        let listed = help
            .split(|c: char| c.is_whitespace() || c == ',')
            .any(|word| word == "--data-dir");
        assert!(
            listed,
            "`{}` does not list --data-dir:\n{help}",
            args.join(" ")
        );
    }
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn config_is_read_only_from_the_data_dir() {
    let scratch = Scratch::new();
    let decoy = "[station]\ncallsign = \"K9ZZZ\"\ngrid_square = \"EM10\"\n";
    for dir in [
        scratch.home_pancetta(),
        scratch.home.path().join(".config").join("pancetta"),
        scratch.cwd.path().to_path_buf(),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pancetta.toml"), decoy).unwrap();
    }
    let path = scratch.write_data_config(DEMO_STATION);

    let out = scratch.with_data_dir(&["config", "--validate"]);
    assert_success(&out);
    let text = stdout(&out);
    assert_eq!(
        config_file_lines(&text),
        vec![format!("Config file: {}", path.display())],
        "{text}"
    );
    assert!(text.contains("Configuration validation: PASS"), "{text}");
}

#[test]
fn an_empty_data_dir_is_the_only_place_searched() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();

    let out = scratch.with_data_dir(&["config", "--validate"]);
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("No config file found"), "{text}");
    assert_eq!(
        searched_dirs(&text),
        vec![dir.display().to_string()],
        "{text}"
    );
    let hint = format!(
        "Run `pancetta --data-dir {} setup` to create {}.",
        dir.display(),
        dir.join("pancetta.toml").display()
    );
    assert!(text.contains(&hint), "missing {hint:?} in:\n{text}");
    assert!(text.contains("Configuration validation: PASS"), "{text}");
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn an_explicit_config_still_wins_over_the_data_dir() {
    let scratch = Scratch::new();
    let decoy = scratch.write_data_config("[station]\ncallsign = \"K9ZZZ\"\n");
    let explicit = scratch.cwd.path().join("station.toml");
    fs::write(&explicit, DEMO_STATION).unwrap();

    let out = scratch
        .command()
        .arg("--config")
        .arg(&explicit)
        .arg("--data-dir")
        .arg(scratch.data_dir())
        .args(["config", "--validate"])
        .output()
        .unwrap();
    assert_success(&out);
    let text = stdout(&out);
    assert_eq!(
        config_file_lines(&text),
        vec![format!("Config file: {}", explicit.display())],
        "{text}"
    );
    assert!(
        !text.contains(&decoy.display().to_string()),
        "the data-dir config was read: {text}"
    );
    assert!(
        !log_files(&scratch.data_dir()).is_empty(),
        "no log file in {}/logs",
        scratch.data_dir().display()
    );
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn logs_are_written_under_the_data_dir() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();

    let out = scratch.with_data_dir(&["info"]);
    assert_success(&out);
    let files = log_files(&dir);
    assert!(
        files.iter().any(|f| f.starts_with("pancetta.log.")),
        "no daily log in {}/logs: {files:?}",
        dir.display()
    );
    let logs = log_text(&dir);
    let line = format!("Data directory: {}", dir.display());
    assert!(logs.contains(&line), "missing {line:?} in:\n{logs}");
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn import_and_export_use_the_data_dir_logbook() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    let file = scratch.home.path().join("contacts.adi");
    fs::write(&file, wsjtx_file("W1AW")).unwrap();

    let out = scratch
        .command()
        .arg("--data-dir")
        .arg(&dir)
        .arg("import")
        .arg(&file)
        .output()
        .unwrap();
    assert_success(&out);
    let logbook = fs::read_to_string(dir.join("qsos.adi")).unwrap();
    assert!(logbook.contains("W1AW"), "{logbook}");
    let receipts: Vec<_> = fs::read_dir(dir.join("imports"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(
        !receipts.is_empty(),
        "no receipt in {}/imports",
        dir.display()
    );

    let out = scratch.with_data_dir(&["export"]);
    assert_success(&out);
    let exported = stdout(&out);
    assert!(exported.contains("W1AW"), "{exported}");
    assert!(exported.contains("<EOR>"), "{exported}");
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn a_tilde_or_relative_data_dir_resolves_to_an_absolute_path() {
    let scratch = Scratch::new();

    // No shell runs here, so the binary itself sees the literal `~/x`.
    let out = scratch
        .command()
        .args(["--data-dir", "~/x", "info"])
        .output()
        .unwrap();
    assert_success(&out);
    let tilde_dir = scratch.home.path().join("x");
    assert!(
        !log_files(&tilde_dir).is_empty(),
        "no log in {}/logs",
        tilde_dir.display()
    );
    let line = format!("Data directory: {}", tilde_dir.display());
    let logs = log_text(&tilde_dir);
    assert!(logs.contains(&line), "missing {line:?} in:\n{logs}");
    assert!(
        !scratch.cwd.path().join("~").exists(),
        "`~` taken literally"
    );

    let out = scratch
        .command()
        .args(["--data-dir", "rel", "info"])
        .output()
        .unwrap();
    assert_success(&out);
    let rel_dir = scratch.cwd.path().join("rel");
    assert!(
        !log_files(&rel_dir).is_empty(),
        "no log in {}/logs",
        rel_dir.display()
    );
    let logs = log_text(&rel_dir);
    let logged = logs
        .lines()
        .find_map(|l| l.split_once("Data directory: ").map(|(_, p)| p.trim()))
        .unwrap_or_else(|| panic!("no Data directory line in:\n{logs}"));
    let logged = Path::new(logged);
    assert!(logged.is_absolute(), "{}", logged.display());
    assert!(logged.ends_with("rel"), "{}", logged.display());
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn an_empty_data_dir_value_is_rejected() {
    let scratch = Scratch::new();
    let out = scratch
        .command()
        .args(["--data-dir", "", "info"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("--data-dir must name a directory"),
        "stderr: {}",
        stderr(&out)
    );
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn a_station_keeps_its_logbook_and_state_in_the_data_dir() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    scratch.write_data_config(DEMO_STATION);

    let mut station = scratch.spawn_station();
    station.wait_until("QSO component ready", Duration::from_secs(60), || {
        log_text(&dir).contains("QSO component ready (callsign=K1ABC")
    });
    // The hardware-tier probe writes its cache shortly after start.
    station.wait_until("tier_cache.json", Duration::from_secs(120), || {
        dir.join("tier_cache.json").exists()
    });
    for name in [
        "qsos.adi",
        "qso.db",
        "tier_cache.json",
        "pancetta.lock",
        "pancetta.pid",
    ] {
        assert!(dir.join(name).exists(), "{} missing", name);
    }
    let pid = fs::read_to_string(dir.join("pancetta.pid")).unwrap();
    assert_eq!(pid.trim(), station.child.0.id().to_string());
    drop(station);
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn station_keys_live_in_the_data_dir() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();

    let out = scratch.with_data_dir(&["identity"]);
    assert_success(&out);
    let text = stdout(&out);
    let key_dir = dir.join("agent");
    let line = format!("Key dir:   {}", key_dir.display());
    assert!(text.contains(&line), "missing {line:?} in:\n{text}");
    let keys = fs::read_dir(&key_dir)
        .unwrap_or_else(|e| panic!("{}: {e}", key_dir.display()))
        .count();
    assert!(keys > 0, "no station key in {}", key_dir.display());
    let hint = format!("run `pancetta --data-dir {} pair <code>`", dir.display());
    assert!(text.contains(&hint), "missing {hint:?} in:\n{text}");
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn an_explicit_key_dir_still_wins() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    let key_dir = scratch.data.path().join("keys");
    scratch.write_data_config(&format!(
        "[network.station_agent]\nkey_dir = {}\n",
        toml_path(&key_dir)
    ));

    let out = scratch.with_data_dir(&["identity"]);
    assert_success(&out);
    let text = stdout(&out);
    let line = format!("Key dir:   {}", key_dir.display());
    assert!(text.contains(&line), "missing {line:?} in:\n{text}");
    assert!(key_dir.exists(), "{} not created", key_dir.display());
    assert!(!dir.join("agent").exists(), "keys went to the data dir");
    scratch.assert_home_pancetta_untouched();
}

#[test]
fn the_upload_outbox_lives_in_the_data_dir() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    scratch.write_data_config(&format!(
        "{DEMO_STATION}\n[network.clublog]\nenabled = true\napi_key = \"dummy-key\"\n\
         callsign = \"K1ABC\"\nemail = \"op@example.com\"\npassword = \"dummy\"\n"
    ));
    fs::write(dir.join("upload_outbox.json"), "not json").unwrap();
    let home_dir = scratch.home_pancetta();
    fs::create_dir_all(&home_dir).unwrap();
    let home_outbox = home_dir.join("upload_outbox.json");
    fs::write(&home_outbox, "not json").unwrap();

    let quarantined = |d: &Path| -> Vec<String> {
        fs::read_dir(d)
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("upload_outbox.json.corrupt-"))
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut station = scratch.spawn_station();
    station.wait_until("the outbox quarantine", Duration::from_secs(60), || {
        !quarantined(&dir).is_empty()
    });
    drop(station);

    assert_eq!(fs::read(&home_outbox).unwrap(), b"not json");
    assert!(
        quarantined(&home_dir).is_empty(),
        "~/.pancetta outbox was quarantined: {:?}",
        quarantined(&home_dir)
    );
}
