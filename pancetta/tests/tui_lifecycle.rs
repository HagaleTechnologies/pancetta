//! Real-binary terminal lifecycle regressions, without audio or rig hardware.
#![cfg(unix)]
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn lifecycle(answer_queries: bool) {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("pancetta.toml");
    std::fs::write(&config, "[station]\ncallsign = \"K5ARH\"\ngrid_square = \"EM12\"\n[rig.interface]\nenabled = false\n").unwrap();
    let wavs = home.path().join("wavs");
    std::fs::create_dir(&wavs).unwrap();
    let mut wav = hound::WavWriter::create(
        wavs.join("test.wav"),
        hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for _ in 0..12000 {
        wav.write_sample(0i16).unwrap();
    }
    wav.finalize().unwrap();

    let (mut master, slave, before) = unsafe {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size
            ),
            0
        );
        let mut term = std::mem::zeroed::<libc::termios>();
        assert_eq!(libc::tcgetattr(slave, &mut term), 0);
        assert_eq!(libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK), 0);
        (File::from_raw_fd(master), File::from_raw_fd(slave), term)
    };
    let mut child = Child(
        Command::new(env!("CARGO_BIN_EXE_pancetta"))
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .env("XDG_CACHE_HOME", home.path())
            .env("TERM", "xterm-256color")
            .env("PANCETTA_MACOS_RELAUNCH", "0")
            .args(["--config"])
            .arg(config)
            .arg("--replay")
            .arg(wavs)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let mut output = Vec::new();
    let mut scan = 0;
    let mut quit_prompted = false;
    let mut quit_sent = None;
    let status = loop {
        let mut buf = [0u8; 16384];
        match master.read(&mut buf) {
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(e) => panic!("{e}"),
        }
        if answer_queries {
            while let Some(i) = output[scan..].windows(4).position(|s| s == b"\x1b[6n") {
                scan += i + 4;
                master.write_all(b"\x1b[1;1R").unwrap();
            }
            if !quit_prompted && output.windows(6).any(|s| s == b"\x1b[?25l") {
                // Let the first frame render, then quit through the existing confirmation.
                std::thread::sleep(Duration::from_millis(200));
                master.write_all(b"q").unwrap();
                quit_prompted = true;
            }
        }
        if answer_queries
            && quit_prompted
            && quit_sent.is_none()
            && String::from_utf8_lossy(&output).contains("Quit pancetta?")
        {
            master.write_all(b"y").unwrap();
            quit_sent = Some(Instant::now());
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(75),
            "child hung; terminal output: {}",
            String::from_utf8_lossy(&output)
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    // Drain bytes written between the last read and process exit.
    let mut tail = [0; 16384];
    while let Ok(n) = master.read(&mut tail) {
        if n == 0 {
            break;
        }
        output.extend_from_slice(&tail[..n]);
    }
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("\x1b[?1049h"), "{text} {stderr}");
    assert!(text.contains("\x1b[?1049l"), "{text} {stderr}");
    assert!(text.contains("\x1b[?25h"), "{text} {stderr}");
    let after = unsafe {
        let mut term = std::mem::zeroed::<libc::termios>();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut term), 0);
        term
    };
    assert_eq!(before.c_lflag, after.c_lflag);
    assert_eq!(before.c_iflag, after.c_iflag);
    assert_eq!(before.c_oflag, after.c_oflag);
    assert_eq!(before.c_cflag, after.c_cflag);
    assert_eq!(before.c_cc, after.c_cc);
    if answer_queries {
        assert!(
            quit_sent
                .expect("normal quit was never confirmed")
                .elapsed()
                < Duration::from_secs(10),
            "process should exit promptly after confirmed quit, rather than replay exhaustion"
        );
        assert_eq!(status.code(), Some(0), "{stderr}");
    } else {
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("TUI failed:") && stderr.contains("cursor"),
            "missing full TUI cause: {stderr}"
        );
    }
}
#[test]
fn pan333_startup_failure_restores_terminal_and_exits_one() {
    lifecycle(false);
}
#[test]
fn pan333_normal_quit_restores_terminal_and_exits_zero() {
    lifecycle(true);
}
