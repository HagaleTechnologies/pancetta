//! `pancetta doctor` — fast, independent station-health checks, each with a
//! one-line fix. The universal first answer to "it doesn't work" and the
//! first line of every troubleshooting doc. Checks are data
//! ([`DoctorCheck`] entries in [`build_checks`]) so adding one is a
//! one-liner for later phases.

use pancetta_config::Config;
use std::path::PathBuf;
use std::time::Duration;

/// Outcome status of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Ready.
    Pass,
    /// Not OK, but not fatal to a decode-only station (or unverifiable here).
    Warn,
    /// Broken. If the check is `hard`, doctor exits non-zero.
    Fail,
}

/// Result of running one check: one line of detail, one line of fix.
pub struct CheckOutcome {
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

/// One doctor check, kept as data so later phases add checks as one-liners
/// in [`build_checks`]. `hard` checks gate the exit code; soft checks print.
pub struct DoctorCheck {
    pub name: &'static str,
    pub hard: bool,
    pub run: Box<dyn Fn(&DoctorCtx) -> CheckOutcome>,
}

/// Shared context: the config is loaded ONCE here so each check stays fast
/// and independent. `config` is `Some` whenever the file parsed, even if
/// validation failed (`config_error` carries the parse/validate error text)
/// so later checks can still use e.g. the device names.
pub struct DoctorCtx {
    pub config_path: PathBuf,
    pub config: Option<Config>,
    pub config_error: Option<String>,
}

pub fn build_ctx(config_override: Option<PathBuf>) -> DoctorCtx {
    let config_path = config_override.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".pancetta")
            .join("pancetta.toml")
    });
    if !config_path.exists() {
        return DoctorCtx {
            config_path,
            config: None,
            config_error: None,
        };
    }
    match Config::load_from_file(&config_path) {
        Ok(cfg) => {
            let config_error = cfg.validate().err().map(|e| e.to_string());
            DoctorCtx {
                config_path,
                config: Some(cfg),
                config_error,
            }
        }
        Err(e) => DoctorCtx {
            config_path,
            config: None,
            config_error: Some(e.to_string()),
        },
    }
}

/// The check registry. Order = print order. Phase-5 additions: append here.
pub fn build_checks() -> Vec<DoctorCheck> {
    vec![
        check_config(),
        check_clock(),
        check_audio_device(),
        check_audio_level(),
        check_decoder(),
        check_rigctld(),
        check_submodule(),
    ]
}

pub fn status_label(s: CheckStatus) -> &'static str {
    match s {
        CheckStatus::Pass => "PASS",
        CheckStatus::Warn => "WARN",
        CheckStatus::Fail => "FAIL",
    }
}

/// Non-zero iff any HARD check FAILed. Warns and soft fails never gate.
pub fn doctor_exit_code(results: &[(bool, CheckStatus)]) -> i32 {
    if results
        .iter()
        .any(|(hard, s)| *hard && *s == CheckStatus::Fail)
    {
        1
    } else {
        0
    }
}

fn not_configured() -> CheckOutcome {
    CheckOutcome {
        status: CheckStatus::Warn,
        detail: "not configured — run the wizard".to_string(),
        fix: Some("run `pancetta` (first-run wizard) or `pancetta setup`".to_string()),
    }
}

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_EPOCH_DELTA: f64 = 2_208_988_800.0;

/// Convert an 8-byte NTP timestamp (32.32 fixed point, seconds since 1900)
/// to Unix seconds as f64.
pub(crate) fn ntp_ts_to_unix_f64(b: &[u8]) -> f64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as f64 / 4_294_967_296.0;
    secs + frac - NTP_UNIX_EPOCH_DELTA
}

/// RFC 4330 clock offset from the four timestamps:
/// T1 local send, T2 server receive, T3 server transmit, T4 local receive.
/// Positive = the local clock is BEHIND the server.
pub(crate) fn sntp_offset(t1: f64, t2: f64, t3: f64, t4: f64) -> f64 {
    ((t2 - t1) + (t3 - t4)) / 2.0
}

fn unix_now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One-shot SNTP (RFC 4330) query over UDP. Returns the clock offset in
/// seconds. Hand-rolled on purpose: a 48-byte packet is not worth a crate.
pub(crate) fn sntp_clock_offset(server: &str, timeout: Duration) -> anyhow::Result<f64> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.connect(server)?;

    // 48-byte client request: LI=0, VN=4, Mode=3 (client) → first byte 0x23.
    let mut req = [0u8; 48];
    req[0] = 0x23;
    let t1 = unix_now_f64();
    sock.send(&req)?;
    let mut resp = [0u8; 48];
    let n = sock.recv(&mut resp)?;
    let t4 = unix_now_f64();
    anyhow::ensure!(n >= 48, "short SNTP response ({n} bytes)");
    let mode = resp[0] & 0x07;
    anyhow::ensure!(
        mode == 4 || mode == 5,
        "not an SNTP server response (mode {mode})"
    );
    let t2 = ntp_ts_to_unix_f64(&resp[32..40]); // Receive timestamp
    let t3 = ntp_ts_to_unix_f64(&resp[40..48]); // Transmit timestamp
    anyhow::ensure!(t3 > 0.0, "SNTP transmit timestamp is zero");
    Ok(sntp_offset(t1, t2, t3, t4))
}

pub(crate) fn check_config() -> DoctorCheck {
    DoctorCheck {
        name: "config",
        hard: true,
        run: Box::new(|ctx| {
            if !ctx.config_path.exists() {
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!(
                        "not configured — {} does not exist",
                        ctx.config_path.display()
                    ),
                    fix: Some("run `pancetta` (first-run wizard) or `pancetta setup`".to_string()),
                };
            }
            if let Some(e) = &ctx.config_error {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("does not load/validate: {e}"),
                    fix: Some(
                        "run `pancetta setup` (rewrites the file) or fix the TOML by hand"
                            .to_string(),
                    ),
                };
            }
            let cfg = ctx
                .config
                .as_ref()
                .expect("config present when no error recorded");
            if cfg.station.callsign == "N0CALL" {
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "valid, but callsign is still N0CALL (CQ will be refused)".to_string(),
                    fix: Some("run `pancetta setup` and set your callsign".to_string()),
                };
            }
            CheckOutcome {
                status: CheckStatus::Pass,
                detail: format!(
                    "{} / {} — loads and validates",
                    cfg.station.callsign, cfg.station.grid_square
                ),
                fix: None,
            }
        }),
    }
}

fn clock_fix_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "System Settings → General → Date & Time → 'Set time automatically'"
    } else if cfg!(target_os = "windows") {
        "run `w32tm /resync` in an admin prompt (or Settings → Time & Language → Sync now)"
    } else {
        "enable an NTP daemon: `sudo apt install chrony` (or systemd-timesyncd)"
    }
}

pub(crate) fn check_clock() -> DoctorCheck {
    DoctorCheck {
        name: "system clock",
        hard: true,
        run: Box::new(|_ctx| {
            match sntp_clock_offset("pool.ntp.org:123", Duration::from_secs(2)) {
                // FT8 slots are UTC-aligned; past ~1 s decodes fail systematically.
                Ok(offset) if offset.abs() < 1.0 => CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: format!("offset {offset:+.3} s vs pool.ntp.org"),
                    fix: None,
                },
                Ok(offset) => CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("offset {offset:+.2} s — FT8 needs < ~1 s from UTC"),
                    fix: Some(clock_fix_hint().to_string()),
                },
                Err(e) => CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!("could not reach pool.ntp.org ({e}) — clock unverified"),
                    fix: Some("check network; FT8 needs the clock within ~1 s of UTC".to_string()),
                },
            }
        }),
    }
}

pub(crate) fn check_audio_device() -> DoctorCheck {
    DoctorCheck {
        name: "audio input device",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else {
                return not_configured();
            };
            let wanted = &cfg.audio.input_device;
            let inputs = pancetta_audio::device::list_input_devices();
            if inputs.is_empty() {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: "no audio input devices found on this host".to_string(),
                    fix: Some("plug in the rig's USB CODEC, then re-run".to_string()),
                };
            }
            if wanted.eq_ignore_ascii_case("default") {
                let name = inputs
                    .iter()
                    .find(|(_, is_default)| *is_default)
                    .map(|(n, _)| n.as_str())
                    .unwrap_or("<none>");
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!(
                        "using system default input ('{name}') — works, but fragile for a rig"
                    ),
                    fix: Some(
                        "set [audio] input_device to the rig CODEC name \
                         (run `pancetta test-audio --list`)"
                            .to_string(),
                    ),
                };
            }
            // Same resolution semantics as the live audio stream: case-
            // insensitive SUBSTRING match (find_input_device_by_name).
            let found = inputs
                .iter()
                .any(|(n, _)| n.to_lowercase().contains(&wanted.to_lowercase()));
            if found {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: format!("'{wanted}' present"),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("'{wanted}' NOT present (startup would silently fall back)"),
                    fix: Some(
                        "run `pancetta test-audio --list` and copy the exact name into \
                         [audio] input_device"
                            .to_string(),
                    ),
                }
            }
        }),
    }
}

/// Capture `dur` of input audio and return (rms, peak) of the samples.
fn capture_rms(cfg: &Config, dur: Duration) -> anyhow::Result<(f32, f32)> {
    use pancetta_audio::{AudioManager, AudioManagerConfig};
    let mut mgr = AudioManager::with_config(AudioManagerConfig {
        input_device: Some(cfg.audio.input_device.clone()),
        output_device: Some(cfg.audio.output_device.clone()),
        sample_rate: cfg.audio.sample_rate,
        buffer_size: cfg.audio.buffer_size as usize,
        channels: cfg.audio.input_channels as u16,
        ..Default::default()
    })?;
    mgr.start()?;
    let deadline = std::time::Instant::now() + dur;
    let (mut sum_sq, mut n, mut peak) = (0f64, 0usize, 0f32);
    while std::time::Instant::now() < deadline {
        match mgr.process_audio()? {
            Some(samples) => {
                for s in &samples {
                    sum_sq += (*s as f64) * (*s as f64);
                    peak = peak.max(s.abs());
                }
                n += samples.len();
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let _ = mgr.stop();
    anyhow::ensure!(n > 0, "no samples captured in {dur:?}");
    Ok(((sum_sq / n as f64).sqrt() as f32, peak))
}

pub(crate) fn check_audio_level() -> DoctorCheck {
    DoctorCheck {
        name: "audio level (2 s)",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else {
                return not_configured();
            };
            match capture_rms(cfg, Duration::from_secs(2)) {
                Ok((rms, peak)) => {
                    let dbfs = 20.0 * rms.max(1e-9).log10();
                    if peak < 1e-6 {
                        CheckOutcome {
                            status: CheckStatus::Fail,
                            detail: "FLAT LINE — no signal reaching the input at all".to_string(),
                            fix: Some(
                                "wrong input device, OS-muted, or rig AF off — \
                                 `pancetta test-audio --list`, check OS input level, \
                                 turn up the rig's USB audio out"
                                    .to_string(),
                            ),
                        }
                    } else {
                        CheckOutcome {
                            status: CheckStatus::Pass,
                            detail: format!("rms {dbfs:.0} dBFS, peak {peak:.3}"),
                            fix: None,
                        }
                    }
                }
                Err(e) => CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("could not capture: {e}"),
                    fix: Some(
                        "run `pancetta test-audio --list`; close other apps holding the device \
                         (including a running pancetta)"
                            .to_string(),
                    ),
                },
            }
        }),
    }
}

pub(crate) fn check_decoder() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib decoder",
        // Soft by invariant: the stub build is degraded, never fatal.
        hard: false,
        run: Box::new(|_ctx| {
            if pancetta_ft8::ft8lib_is_available() {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "native C decoder compiled in".to_string(),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "STUB build — pure-Rust decoder only (degraded decode recall)"
                        .to_string(),
                    fix: Some("git submodule update --init && cargo build --release".to_string()),
                }
            }
        }),
    }
}

fn hamlib_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install hamlib"
    } else if cfg!(target_os = "windows") {
        "install hamlib from https://hamlib.github.io and put rigctld.exe on PATH"
    } else {
        "sudo apt install libhamlib-utils"
    }
}

pub(crate) fn check_rigctld() -> DoctorCheck {
    DoctorCheck {
        name: "rig / rigctld",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else {
                return not_configured();
            };
            if !cfg.rig.interface.enabled {
                return CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "rig disabled — decode-only (mock rig, no PTT)".to_string(),
                    fix: Some("to go on-air: `pancetta setup` and enable rig control".to_string()),
                };
            }
            // Model must map to a hamlib ID or the coordinator can't spawn rigctld
            // (hamlib.rs:252-258 warns and gives up).
            if pancetta_lib::coordinator::ApplicationCoordinator::hamlib_model_id(&cfg.rig.model)
                .is_none()
            {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!(
                        "unknown rig model '{}' — pancetta cannot spawn rigctld for it",
                        cfg.rig.model
                    ),
                    fix: Some(
                        "run `pancetta setup` and set a supported model (FTdx10, IC-7300, …), \
                         or run an external rigctld and set RIGCTLD_HOST/RIGCTLD_PORT"
                            .to_string(),
                    ),
                };
            }
            // Binary on PATH — pancetta spawns `rigctld` at startup (hamlib.rs:222).
            let on_path = std::process::Command::new("rigctld")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok();
            if !on_path {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: "rig enabled but `rigctld` is not on PATH".to_string(),
                    fix: Some(hamlib_install_hint().to_string()),
                };
            }
            // Same host/port conventions as the coordinator (hamlib.rs:150-155).
            let host = std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
            let port: u16 = std::env::var("RIGCTLD_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(4532);
            use std::net::ToSocketAddrs;
            let reachable = format!("{host}:{port}")
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .map(|a| {
                    std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok()
                })
                .unwrap_or(false);
            let detail = if reachable {
                format!("rigctld on PATH; already listening on {host}:{port}")
            } else {
                format!(
                    "rigctld on PATH; nothing on {host}:{port} yet (pancetta spawns it at startup)"
                )
            };
            CheckOutcome {
                status: CheckStatus::Pass,
                detail,
                fix: None,
            }
        }),
    }
}

pub(crate) fn check_submodule() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib submodule",
        hard: false,
        run: Box::new(|_ctx| {
            // Only meaningful when run from a source-checkout root — probes
            // the same path pancetta-ft8/build.rs uses at compile time
            // (vendor/ft8_lib/ft8/constants.c).
            if !std::path::Path::new("pancetta-ft8").exists() {
                return CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "not run from a source checkout (skipped)".to_string(),
                    fix: None,
                };
            }
            if std::path::Path::new("pancetta-ft8/vendor/ft8_lib/ft8/constants.c").exists() {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "vendor sources present".to_string(),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "submodule not initialized — the NEXT build will be a stub build"
                        .to_string(),
                    fix: Some("git submodule update --init".to_string()),
                }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exit_code_is_nonzero_only_for_hard_failures() {
        use CheckStatus::*;
        assert_eq!(doctor_exit_code(&[(true, Pass), (false, Fail)]), 0); // soft fail ok
        assert_eq!(doctor_exit_code(&[(true, Warn), (false, Warn)]), 0); // warns ok
        assert_eq!(doctor_exit_code(&[(true, Fail)]), 1); // hard fail
        assert_eq!(doctor_exit_code(&[]), 0);
    }

    #[test]
    fn config_check_degrades_when_no_config_file() {
        let ctx = build_ctx(Some(PathBuf::from("/nonexistent/dir/pancetta.toml")));
        let outcome = (check_config().run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Warn);
        assert!(outcome.detail.contains("not configured"));
        assert!(outcome.fix.unwrap().contains("wizard"));
    }

    #[test]
    fn config_check_fails_hard_on_unparseable_file() {
        let dir = std::env::temp_dir().join(format!("pancetta-doctor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        std::fs::write(&path, "[station\nthis is not toml").unwrap();
        let ctx = build_ctx(Some(path));
        let outcome = (check_config().run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Fail);
        assert!(check_config().hard);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ntp_timestamp_conversion_handles_epoch_and_fraction() {
        // NTP epoch is 1900-01-01; Unix is 1970-01-01; delta 2_208_988_800 s.
        let unix_zero: [u8; 8] = [0x83, 0xAA, 0x7E, 0x80, 0, 0, 0, 0]; // 2_208_988_800.0
        assert_eq!(ntp_ts_to_unix_f64(&unix_zero), 0.0);
        // +1 second and a half-fraction (0x8000_0000 / 2^32 = 0.5) → 1.5.
        let one_and_half: [u8; 8] = [0x83, 0xAA, 0x7E, 0x81, 0x80, 0, 0, 0];
        assert!((ntp_ts_to_unix_f64(&one_and_half) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn sntp_offset_recovers_skew_independent_of_symmetric_delay() {
        // Local clock 5 s slow, 200 ms symmetric round trip:
        // T1=100.0 (local send), T2=T3=105.1 (server), T4=100.2 (local recv).
        let offset = sntp_offset(100.0, 105.1, 105.1, 100.2);
        assert!((offset - 5.0).abs() < 1e-9);
        // Zero skew, only delay → offset 0.
        assert!(sntp_offset(100.0, 100.1, 100.1, 100.2).abs() < 1e-9);
    }
}
