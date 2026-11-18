//! A PTY sends the same terminal bytes as an SSH attachment to the station TUI.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn attached_terminal_reaches_the_running_station() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("station.toml");
    std::fs::write(
        &path,
        "[station]\ncallsign = 'K5ARH'\ngrid_square = 'EM10'\n",
    )
    .unwrap();
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors; the child receives only
    // the slave, and the parent owns the master after spawn.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // `*mut` on macOS, `*const` on Linux; null_mut coerces to both.
                std::ptr::null_mut(),
            )
        },
        0
    );
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the slave descriptor and the winsize structure are valid.
    assert_eq!(unsafe { libc::ioctl(slave_fd, libc::TIOCSWINSZ, &size) }, 0);
    // SAFETY: openpty returned valid owned descriptors.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let stdout = slave.try_clone().unwrap();
    let stderr = slave.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_pancetta"));
    command
        .args(["--config", path.to_str().unwrap(), "--no-audio"])
        .env("HOME", home.path())
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: pre_exec performs only async-signal-safe descriptor/session calls.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // SAFETY: fcntl updates flags on the valid master descriptor.
    unsafe {
        libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut screen = Vec::new();
    let mut answered_cursor = false;
    let mut opened_health = false;
    let mut sent = false;
    let mut saved = false;
    while Instant::now() < deadline {
        let mut chunk = [0; 4096];
        if let Ok(n) = master.read(&mut chunk) {
            screen.extend_from_slice(&chunk[..n]);
        }
        if !answered_cursor && screen.windows(4).any(|bytes| bytes == b"\x1b[6n") {
            master.write_all(b"\x1b[1;1R").unwrap();
            answered_cursor = true;
        }
        if !opened_health && !screen.is_empty() {
            master.write_all(b"S").unwrap();
            opened_health = true;
        }
        if opened_health && !sent && String::from_utf8_lossy(&screen).contains("drive 0 dB") {
            master.write_all(b"\x1b[18~").unwrap();
            sent = true;
        }
        if sent
            && std::fs::read_to_string(&path)
                .unwrap()
                .contains("output_gain_db = -1")
        {
            saved = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(sent, "TUI did not render the initial drive");
    assert!(saved, "F7 did not save -1 dB");
}
