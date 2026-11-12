//! PAN-339: exercise the real CLI against an isolated rigctld transcript.
use assert_cmd::Command;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Daemon {
    port: u16,
    commands: Arc<Mutex<Vec<(String, Instant)>>>,
    task: std::thread::JoinHandle<()>,
}
impl Daemon {
    fn start(freq: &str, split: &str, tx: &str, on: &str, off: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let log = commands.clone();
        let (freq, split, tx, on, off) = (
            freq.to_owned(),
            split.to_owned(),
            tx.to_owned(),
            on.to_owned(),
            off.to_owned(),
        );
        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let cmd = line.trim().to_owned();
                log.lock().unwrap().push((cmd.clone(), Instant::now()));
                let response = match cmd.as_str() {
                    "f" => &freq,
                    "s" => &split,
                    "i" => &tx,
                    "T 1" => &on,
                    "T 0" => &off,
                    other => panic!("unexpected command: {other}"),
                };
                // Leave time for pipe/interrupt tests to act before key acknowledgement.
                if cmd == "T 1" {
                    std::thread::sleep(Duration::from_millis(100));
                }
                writeln!(stream, "{response}").unwrap();
            }
        });
        Self {
            port,
            commands,
            task,
        }
    }
    fn finish(self) -> Vec<(String, Instant)> {
        self.task.join().unwrap();
        Arc::try_unwrap(self.commands)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}
fn raw_command(
    port: u16,
    enabled: bool,
    method: &str,
    extra: &str,
    ptt: bool,
) -> (tempfile::TempDir, std::process::Command) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("station.toml");
    std::fs::write(&config, format!("[rig.interface]\nenabled = {enabled}\nport = '/nonexistent/pan339'\n[rig.ptt]\nmethod = '{method}'\n{extra}")).unwrap();
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
    cmd.current_dir(dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .env("XDG_CACHE_HOME", dir.path())
        .env_remove("PANCETTA_CALLSIGN")
        .env("RIGCTLD_HOST", "127.0.0.1")
        .env("RIGCTLD_PORT", port.to_string())
        .args(["--headless", "--config"])
        .arg(config)
        .arg("test-rig");
    if ptt {
        cmd.arg("--ptt");
    }
    (dir, cmd)
}
fn command(
    port: u16,
    enabled: bool,
    method: &str,
    extra: &str,
    ptt: bool,
) -> (tempfile::TempDir, Command) {
    let (dir, raw) = raw_command(port, enabled, method, extra, ptt);
    let mut cmd = Command::from_std(raw);
    cmd.timeout(Duration::from_secs(30));
    (dir, cmd)
}
fn run(
    d: Daemon,
    method: &str,
    extra: &str,
    ptt: bool,
    success: bool,
    message: &str,
) -> Vec<(String, Instant)> {
    let (_dir, mut cmd) = command(d.port, true, method, extra, ptt);
    let out = cmd.output().unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(if success { 0 } else { 1 }),
        "{all}"
    );
    assert!(all.contains(message), "{all}");
    assert_eq!(all.contains("Rig test complete."), success, "{all}");
    d.finish()
}
#[test]
fn missing_daemon_exits_one() {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let (_dir, mut cmd) = command(port, true, "cat", "", false);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot connect to rigctld at"));
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("127.0.0.1:{port}")));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Rig test complete."));
}
#[test]
fn disabled_control_fails() {
    let (_dir, mut cmd) = command(1, false, "cat", "", false);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("rig control is disabled"));
}
#[test]
fn bad_frequency_fails() {
    run(
        Daemon::start("invalid", "0\nVFOA", "", "", ""),
        "cat",
        "",
        false,
        false,
        "Reading frequency",
    );
}
#[test]
fn unsupported_ptt_fails() {
    for method in ["none", "vox"] {
        let log = run(
            Daemon::start("14074000", "0\nVFOA", "", "", ""),
            method,
            "",
            true,
            false,
            "cannot be tested",
        );
        assert!(!log.iter().any(|(c, _)| c.starts_with("T ")));
    }
}
#[test]
fn link_only_uses_rigctld_without_serial_or_ptt() {
    for method in ["cat", "serial", "none", "vox"] {
        let log = run(
            Daemon::start("14074000", "", "", "", ""),
            method,
            "",
            false,
            true,
            "PTT test: skipped",
        );
        assert!(log.iter().all(|(c, _)| c == "f"));
    }
}
#[test]
fn cat_ptt_keys_then_unkeys() {
    let log = run(
        Daemon::start("14074000", "0\nVFOA", "", "RPRT 0", "RPRT 0"),
        "cat",
        "",
        true,
        true,
        "PTT OFF",
    );
    let on = log.iter().find(|(c, _)| c == "T 1").unwrap().1;
    let off = log.iter().find(|(c, _)| c == "T 0").unwrap().1;
    assert!(off.duration_since(on) >= Duration::from_secs(1));
    assert_eq!(
        log.iter()
            .filter(|(c, _)| c.starts_with("T "))
            .map(|(c, _)| c.as_str())
            .collect::<Vec<_>>(),
        ["T 1", "T 0"]
    );
}
#[test]
fn ptt_band_gate() {
    for (freq, split, tx, extra, allowed) in [
        ("30000000", "0\nVFOA", "", "", false),
        ("14074000", "1\nVFOB", "30000000", "", false),
        ("30000000", "1\nVFOB", "14074000", "", true),
        ("14074000", "RPRT -1", "", "", false),
        ("14074000", "1\nVFOB", "0", "", false),
        ("30000000", "0\nVFOA", "", "[rig.frequency.band_plan]\nedge_warnings = false", true),
        ("7250000", "0\nVFOA", "", "[rig.frequency.band_plan]\nregion = 2", true),
        ("7250000", "0\nVFOA", "", "[rig.frequency.band_plan]\nregion = 1", false),
        ("14074000", "0\nVFOA", "", "[rig.frequency.band_plan]\n[rig.frequency.band_plan.custom_bands.'20m']\nname = 'test'\ndefault_mode = 'USB'\nband_type = 'HF'\nranges = [{start = 14080000, end = 14090000, modes = []}]", false),
    ] {
        let log = run(Daemon::start(freq, split, tx, "RPRT 0", "RPRT 0"), "cat", extra, true, allowed, if allowed {"PTT OFF"} else {"Rig test failed"});
        assert_eq!(log.iter().any(|(c,_)| c == "T 1"), allowed);
    }
}
#[test]
fn cat_key_failure_attempts_unkey() {
    let log = run(
        Daemon::start("14074000", "0\nVFOA", "", "RPRT -1", "RPRT 0"),
        "cat",
        "",
        true,
        false,
        "PTT ON failed",
    );
    assert!(log.iter().any(|(c, _)| c == "T 0"));
}
#[test]
fn cat_unkey_failure_exits_one() {
    run(
        Daemon::start("14074000", "0\nVFOA", "", "RPRT 0", "RPRT -1"),
        "cat",
        "",
        true,
        false,
        "PTT OFF failed; rig may still be keyed",
    );
}

#[cfg(unix)]
#[test]
fn interrupt_unkeys() {
    use std::process::Stdio;
    let d = Daemon::start("14074000", "0\nVFOA", "", "RPRT 0", "RPRT 0");
    let (_dir, mut cmd) = raw_command(d.port, true, "cat", "", true);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !d.commands.lock().unwrap().iter().any(|(c, _)| c == "T 1") {
        assert!(Instant::now() < deadline, "child never keyed");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Give the CLI time to consume the daemon's key acknowledgement.
    std::thread::sleep(Duration::from_millis(50));
    assert!(std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("child did not exit after interrupt");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("interrupted"));
    let log = d.finish();
    assert!(log.iter().any(|(c, _)| c == "T 0"));
}

#[test]
fn serial_ptt_refuses_before_opening_port() {
    for (split, tx, reason) in [
        (
            "1\nVFOB",
            "30000000",
            "outside the configured amateur band plan",
        ),
        ("RPRT -1", "", "cannot determine split TX frequency"),
    ] {
        let log = run(
            Daemon::start("14074000", split, tx, "", ""),
            "serial",
            "",
            true,
            false,
            reason,
        );
        assert!(!log.iter().any(|(c, _)| c.starts_with("T ")));
    }
}

#[test]
fn cat_key_and_unkey_failure_preserves_both_causes() {
    let d = Daemon::start("14074000", "0\nVFOA", "", "RPRT -1", "RPRT -2");
    let (_dir, mut cmd) = command(d.port, true, "cat", "", true);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("PTT ON failed"), "{error}");
    assert!(
        error.contains("PTT OFF failed; rig may still be keyed"),
        "{error}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("       PTT OFF"));
    d.finish();
}

#[test]
fn malformed_ptt_acknowledgements_fail() {
    for (on, off, message) in [
        ("garbage", "RPRT 0", "PTT ON failed"),
        (
            "RPRT 0",
            "garbage",
            "PTT OFF failed; rig may still be keyed",
        ),
    ] {
        let log = run(
            Daemon::start("14074000", "0\nVFOA", "", on, off),
            "cat",
            "",
            true,
            false,
            message,
        );
        assert!(log.iter().any(|(c, _)| c == "T 0"));
    }
}

#[cfg(unix)]
#[test]
fn closed_stdout_during_pulse_still_unkeys() {
    use std::process::Stdio;
    let d = Daemon::start("14074000", "0\nVFOA", "", "RPRT 0", "RPRT 0");
    let (_dir, mut cmd) = raw_command(d.port, true, "cat", "", true);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !d.commands.lock().unwrap().iter().any(|(c, _)| c == "T 1") {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("child never keyed");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(child.stdout.take());
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("child did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = d.finish();
    assert!(log.iter().any(|(c, _)| c == "T 0"));
}
