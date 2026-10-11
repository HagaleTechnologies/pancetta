//! Real Unix signals must reach coordinator cleanup. The fake rig starts keyed;
//! this checks the direct rigctld protocol, not a physical transmission.
#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Also reap on assertion failure or a startup/shutdown timeout.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct FakeRig {
    port: u16,
    keyed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<io::Result<Vec<String>>>>,
}

impl FakeRig {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let keyed = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let server_keyed = keyed.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(80);
            let stream = loop {
                if server_stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "no rig connection"));
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => return Err(e),
                }
            };
            serve(stream, &server_keyed)
        });
        Self {
            port,
            keyed,
            stop,
            server: Some(server),
        }
    }
}

fn serve(mut stream: TcpStream, keyed: &AtomicBool) -> io::Result<Vec<String>> {
    // On macOS the accepted stream inherits the listener's O_NONBLOCK.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut commands = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            // A successful server result requires connection EOF.
            return Ok(commands);
        }
        match line.trim() {
            "f" => stream.write_all(b"14074000\n")?,
            "T 0" => {
                keyed.store(false, Ordering::Release);
                stream.write_all(b"RPRT 0\n")?;
            }
            command => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected rig command: {command}"),
                ));
            }
        }
        commands.push(line.trim().to_owned());
    }
}

impl Drop for FakeRig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_signal_shutdown(signal: libc::c_int) {
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("pancetta.toml");
    fs::write(
        &config,
        "[station]\ncallsign = \"K5ARH\"\ngrid_square = \"EM12\"\n\
         [rig.interface]\nenabled = true\n",
    )
    .unwrap();
    let mut rig = FakeRig::start();
    // Regular files cannot fill a pipe and stall the child during startup.
    let output_path = scratch.path().join("output.log");
    let output = File::create(&output_path).unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PANCETTA_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(scratch.path())
        .env("HOME", scratch.path())
        .env("USERPROFILE", scratch.path())
        .env("XDG_CONFIG_HOME", scratch.path().join(".config"))
        .env("XDG_DATA_HOME", scratch.path().join(".local/share"))
        .env("XDG_CACHE_HOME", scratch.path().join(".cache"))
        .env("PANCETTA_MOCK_RIG", "1")
        .env("RIGCTLD_HOST", "127.0.0.1")
        .env("RIGCTLD_PORT", rig.port.to_string())
        .env("RUST_LOG", "info")
        .args(["--config"])
        .arg(&config)
        .args(["--headless", "--no-audio"])
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output);
    // Declared after the rig so failure cleanup kills/reaps before joining it.
    let mut child = ChildGuard(command.spawn().unwrap());
    let startup_deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let logs = fs::read_to_string(&output_path).unwrap();
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before readiness:\n{logs}"
        );
        if logs.contains("Entering main application loop") {
            break;
        }
        assert!(
            Instant::now() < startup_deadline,
            "startup timeout:\n{logs}"
        );
        thread::sleep(Duration::from_millis(20));
    }

    assert!(rig.keyed.load(Ordering::Acquire), "rig must start keyed");
    let pid = libc::pid_t::try_from(child.0.id()).unwrap();
    // SAFETY: this is our live, unreaped child PID, never a process group.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    let status = wait_for_exit(&mut child.0, Duration::from_secs(30));
    let logs = fs::read_to_string(&output_path).unwrap();
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "{status:?}:\n{logs}"
    );

    let commands = rig
        .server
        .take()
        .unwrap()
        .join()
        .expect("fake rig thread panicked")
        .expect("fake rig must acknowledge PTT-off and observe EOF");
    assert_eq!(commands, ["f", "T 0"]);
    assert!(!rig.keyed.load(Ordering::Acquire), "rig must be unkeyed");
    let markers = [
        "Starting graceful shutdown",
        "Shutdown: direct rigctld PTT-off succeeded",
        "Graceful shutdown completed",
        "Pancetta shutdown complete",
    ];
    let mut remaining = logs.as_str();
    for marker in markers {
        let index = remaining
            .find(marker)
            .unwrap_or_else(|| panic!("missing or out-of-order {marker}:\n{logs}"));
        remaining = &remaining[index + marker.len()..];
    }
    assert_eq!(logs.contains("Received Ctrl+C"), signal == libc::SIGINT);
}

#[test]
fn sigterm_unkeys_rig_and_completes_shutdown() {
    assert_signal_shutdown(libc::SIGTERM);
}

#[test]
fn sigint_unkeys_rig_and_completes_shutdown() {
    assert_signal_shutdown(libc::SIGINT);
}

#[test]
fn fake_rig_serves_stream_that_inherited_nonblocking() {
    // macOS accept() copies O_NONBLOCK from the listener; force it here.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, _) = listener.accept().unwrap();
    stream.set_nonblocking(true).unwrap();
    let keyed = Arc::new(AtomicBool::new(true));
    let server_keyed = keyed.clone();
    let server = thread::spawn(move || serve(stream, &server_keyed));

    let mut replies = BufReader::new(client.try_clone().unwrap());
    let mut reply = String::new();
    client.write_all(b"f\n").unwrap();
    replies.read_line(&mut reply).unwrap();
    assert_eq!(reply, "14074000\n");
    // Let the server reach its next read before the command arrives.
    thread::sleep(Duration::from_millis(200));
    reply.clear();
    client.write_all(b"T 0\n").unwrap();
    replies.read_line(&mut reply).unwrap();
    assert_eq!(reply, "RPRT 0\n");
    drop(replies);
    drop(client);

    let commands = server.join().unwrap().unwrap();
    assert_eq!(commands, ["f", "T 0"]);
    assert!(!keyed.load(Ordering::Acquire));
}
