//! PAN-376 scenario 2: two live stations cannot share one data directory. A
//! live station holds `<data-dir>/pancetta.lock`; a second live station on
//! the same directory refuses with "another pancetta is running (pid N)".
//! Maintenance subcommands and `--wav` playback never take the lock.
//!
//! Only cross-platform process APIs (`Child::kill`, no signals), so this
//! file compiles for Windows too.

use assert_cmd::Command;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DEMO_WAV: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../assets/demo-wav/170923_082000.wav"
);

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Also reap on assertion failure or a startup timeout.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A scratch `$HOME`, an empty scratch cwd and a scratch parent for a
/// `--data-dir` directory.
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

    /// A `--data-dir` value: `<scratch>/station`.
    fn data_dir(&self) -> PathBuf {
        self.data.path().join("station")
    }

    /// `~/.pancetta` under the scratch home: the default data directory.
    fn home_pancetta(&self) -> PathBuf {
        self.home.path().join(".pancetta")
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
            .env("RUST_LOG", "info")
            .current_dir(self.cwd.path());
        cmd
    }

    fn command(&self) -> Command {
        let mut cmd = Command::from_std(self.std_command());
        cmd.timeout(Duration::from_secs(60));
        cmd
    }

    /// Put a station config with `callsign` in `dir`.
    fn write_station_config(&self, dir: &Path, callsign: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("pancetta.toml"),
            format!("[station]\ncallsign = \"{callsign}\"\ngrid_square = \"FN42\"\n"),
        )
        .unwrap();
    }

    /// `pancetta [--data-dir DIR] --headless --no-audio`, its console output
    /// in a regular file (a file cannot fill and stall the child).
    fn spawn_station(&self, data_dir: Option<&Path>, label: &str) -> Station {
        let output_path = self.data.path().join(format!("{label}-output.log"));
        let output = File::create(&output_path).unwrap();
        let mut cmd = self.std_command();
        if let Some(dir) = data_dir {
            cmd.arg("--data-dir").arg(dir);
        }
        cmd.args(["--headless", "--no-audio"])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        Station {
            child: ChildGuard(cmd.spawn().unwrap()),
            output: output_path,
            dir: data_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.home_pancetta()),
        }
    }
}

struct Station {
    child: ChildGuard,
    output: PathBuf,
    dir: PathBuf,
}

impl Station {
    fn pid(&self) -> u32 {
        self.child.0.id()
    }

    fn console(&self) -> String {
        fs::read_to_string(&self.output).unwrap_or_default()
    }

    fn is_running(&mut self) -> bool {
        self.child.0.try_wait().unwrap().is_none()
    }

    /// Wait until this station holds its directory (its pid in
    /// `pancetta.pid`) and its QSO component is ready with `callsign`.
    fn wait_ready(&mut self, callsign: &str) {
        let ready = format!("QSO component ready (callsign={callsign}");
        let pid = self.pid().to_string();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let pid_file = fs::read_to_string(self.dir.join("pancetta.pid")).unwrap_or_default();
            if pid_file.trim() == pid && log_text(&self.dir).contains(&ready) {
                return;
            }
            if let Some(status) = self.child.0.try_wait().unwrap() {
                panic!(
                    "station {pid} exited ({status}) before it was ready:\n{}",
                    self.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "station {pid} not ready within 60 s (pancetta.pid = {pid_file:?}):\n{}",
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

fn assert_success(what: &str, o: &std::process::Output) {
    assert!(
        o.status.success(),
        "{what}: status {}\nstdout: {}\nstderr: {}",
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

fn read_pid(dir: &Path) -> String {
    fs::read_to_string(dir.join("pancetta.pid"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Run a second live station, which must be refused (exit 1) within 30 s,
/// naming `holder` in its stderr. Returns that stderr.
fn assert_refused(mut cmd: std::process::Command, holder: u32) -> String {
    let started = Instant::now();
    let out = Command::from_std({
        cmd.stdin(Stdio::null());
        cmd
    })
    .timeout(Duration::from_secs(30))
    .output()
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the second station was not refused within 30 s"
    );
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "second station was not refused:\nstdout: {}\nstderr: {err}",
        stdout(&out)
    );
    let refusal = format!("another pancetta is running (pid {holder})");
    assert!(err.contains(&refusal), "missing {refusal:?} in:\n{err}");
    err
}

#[test]
fn a_second_station_on_the_default_data_dir_is_refused() {
    let scratch = Scratch::new();
    let dir = scratch.home_pancetta();
    scratch.write_station_config(&dir, "K1ABC");

    let mut first = scratch.spawn_station(None, "first");
    first.wait_ready("K1ABC");

    let mut second = scratch.std_command();
    second.args(["--headless", "--no-audio"]);
    let err = assert_refused(second, first.pid());
    let refusal = format!(
        "another pancetta is running (pid {}) with data directory {}",
        first.pid(),
        dir.display()
    );
    assert!(err.contains(&refusal), "missing {refusal:?} in:\n{err}");

    let starts = log_text(&dir).matches("Starting Pancetta v").count();
    assert_eq!(starts, 1, "the refused station logged a start");
    assert!(first.is_running(), "the first station stopped");
    assert!(dir.join("pancetta.lock").exists());
    assert_eq!(read_pid(&dir), first.pid().to_string());
}

#[test]
fn the_same_directory_spelled_differently_is_still_refused() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    scratch.write_station_config(&dir, "K1ABC");
    // `x/..` resolves only when `x` exists.
    fs::create_dir_all(scratch.data.path().join("x")).unwrap();

    let mut first = scratch.spawn_station(Some(&dir), "first");
    first.wait_ready("K1ABC");

    let spellings: [OsString; 3] = [
        "station".into(),
        Path::new("x").join("..").join("station").into(),
        Path::new(".").join("station").into(),
    ];
    for spelling in spellings {
        // No `--headless` and no TTY: still refused before anything else.
        let mut second = scratch.std_command();
        second
            .current_dir(scratch.data.path())
            .arg("--data-dir")
            .arg(&spelling)
            .arg("--no-audio");
        let err = assert_refused(second, first.pid());
        // The refusal names the directory as the OS resolves it, not the
        // operator's spelling.
        let named = format!(
            "with data directory {}",
            fs::canonicalize(&dir).unwrap().display()
        );
        assert!(err.contains(&named), "missing {named:?} in:\n{err}");
        assert!(first.is_running(), "first station stopped ({spelling:?})");
    }
    assert_eq!(read_pid(&dir), first.pid().to_string());
}

#[test]
fn stations_on_different_data_dirs_run_side_by_side() {
    let scratch = Scratch::new();
    let home_dir = scratch.home_pancetta();
    let other = scratch.data_dir();
    scratch.write_station_config(&home_dir, "K1ABC");
    scratch.write_station_config(&other, "K2XYZ");

    let mut a = scratch.spawn_station(None, "a");
    a.wait_ready("K1ABC");
    let mut b = scratch.spawn_station(Some(&other), "b");
    b.wait_ready("K2XYZ");

    assert!(a.is_running(), "station A stopped");
    assert!(b.is_running(), "station B stopped");
    assert_eq!(read_pid(&home_dir), a.pid().to_string());
    assert_eq!(read_pid(&other), b.pid().to_string());
    assert!(!log_text(&home_dir).contains("callsign=K2XYZ"));
    assert!(!log_text(&other).contains("callsign=K1ABC"));
}

#[test]
fn a_killed_station_does_not_block_the_next_one() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    scratch.write_station_config(&dir, "K1ABC");

    let mut a = scratch.spawn_station(Some(&dir), "a");
    a.wait_ready("K1ABC");
    let a_pid = a.pid();
    // SIGKILL on Unix, TerminateProcess on Windows: no clean shutdown.
    a.child.0.kill().unwrap();
    a.child.0.wait().unwrap();
    assert_eq!(read_pid(&dir), a_pid.to_string(), "stale pid file expected");

    let mut b = scratch.spawn_station(Some(&dir), "b");
    b.wait_ready("K1ABC");
    assert!(b.is_running());
    assert_eq!(read_pid(&dir), b.pid().to_string());
}

#[test]
fn maintenance_commands_and_wav_playback_run_beside_a_station() {
    let scratch = Scratch::new();
    let dir = scratch.data_dir();
    scratch.write_station_config(&dir, "K1ABC");
    let adif = scratch.home.path().join("contacts.adi");
    fs::write(
        &adif,
        "WSJT-X ADIF Export<eoh>\n<call:4>W1AW <gridsquare:4>FN31 <mode:3>FT8 \
         <qso_date:8>20240101 <time_on:6>120000 <band:3>20m <freq:9>14.075123 <eor>\n",
    )
    .unwrap();
    assert!(Path::new(DEMO_WAV).exists(), "{DEMO_WAV} missing");

    let mut station = scratch.spawn_station(Some(&dir), "station");
    station.wait_ready("K1ABC");

    let runs: Vec<(&str, Vec<OsString>)> = vec![
        ("export", vec!["export".into()]),
        (
            "import --dry-run",
            vec!["import".into(), "--dry-run".into(), adif.clone().into()],
        ),
        (
            "config --validate",
            vec!["config".into(), "--validate".into()],
        ),
        (
            "--headless --wav",
            vec!["--headless".into(), "--wav".into(), DEMO_WAV.into()],
        ),
    ];
    for (what, args) in runs {
        let out = scratch
            .command()
            .arg("--data-dir")
            .arg(&dir)
            .args(args)
            .output()
            .unwrap();
        assert_success(what, &out);
        assert!(
            !stderr(&out).contains("another pancetta is running"),
            "{what} took the instance lock"
        );
        assert!(station.is_running(), "the station stopped during {what}");
    }
    assert_eq!(read_pid(&dir), station.pid().to_string());
}
