//! `pancetta doctor` — fast, independent station-health checks, each with a
//! one-line fix. The universal first answer to "it doesn't work" and the
//! first line of every troubleshooting doc. Checks are data
//! ([`DoctorCheck`] entries in [`build_checks`]) so adding one is a
//! one-liner for later phases.

use crate::mic_access::{microphone_authorization, MicrophoneAuthorization};
use pancetta_config::Config;
use pancetta_lib::clock_skew::{
    clock_fix_hint, sntp_clock_offset, CLOCK_SKEW_FAIL_S, CLOCK_SKEW_WARN_S, NTP_SERVER,
    NTP_SERVER_HOST, SNTP_TIMEOUT,
};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

/// Outcome status of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CheckStatus {
    /// Ready.
    Pass,
    /// Not OK, but not fatal to a decode-only station (or unverifiable here).
    Warn,
    /// A failed prerequisite or advisory probe.
    Fail,
}

/// Result of running one check: one line of detail, one line of fix.
pub struct CheckOutcome {
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

/// One doctor check, kept as data so later phases add checks as one-liners
/// in [`build_checks`]. Each role determines how failure affects readiness.
pub struct DoctorCheck {
    pub name: &'static str,
    pub role: CheckRole,
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
        check_network(),
        check_disk(),
        check_microphone(),
    ]
}

pub fn status_label(s: CheckStatus) -> &'static str {
    match s {
        CheckStatus::Pass => "PASS",
        CheckStatus::Warn => "WARN",
        CheckStatus::Fail => "FAIL",
    }
}

/// Readiness responsibility, independent of human diagnostic wording.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckRole {
    Receive,
    Transmit,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "NOT CONFIGURED")]
    NotConfigured,
    #[serde(rename = "DECODE-ONLY")]
    DecodeOnly,
    #[serde(rename = "ON-AIR READY")]
    OnAirReady,
    #[serde(rename = "NOT READY")]
    NotReady,
}

impl Verdict {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::OnAirReady => 0,
            Self::DecodeOnly => 1,
            Self::NotConfigured => 2,
            Self::NotReady => 3,
        }
    }

    fn result_line(self) -> &'static str {
        match self {
            Self::NotConfigured => "Result: NOT CONFIGURED. Run `pancetta setup`, then re-run `pancetta doctor`.",
            Self::DecodeOnly => "Result: DECODE-ONLY. Receive checks passed; fix the transmit prerequisites above to go on-air.",
            Self::OnAirReady => "Result: ON-AIR READY. Doctor prerequisites passed; PTT and transmit audio are not tested.",
            Self::NotReady => "Result: NOT READY. Fix the failed receive or configuration checks above, then re-run `pancetta doctor`.",
        }
    }
}

#[derive(Serialize)]
pub struct CheckResult {
    name: &'static str,
    status: CheckStatus,
    detail: String,
    fix: Option<String>,
}

#[derive(Serialize)]
pub struct DoctorReport {
    pub verdict: Verdict,
    pub exit_code: i32,
    config_path: String,
    checks: Vec<CheckResult>,
}

impl DoctorReport {
    // Both live probes and controlled fixtures use this aggregation path.
    fn from_results(ctx: &DoctorCtx, results: Vec<(DoctorCheck, CheckOutcome)>) -> Self {
        let verdict = if ctx.config.is_none() && ctx.config_error.is_none() {
            Verdict::NotConfigured
        } else if ctx.config_error.is_some()
            || results.iter().any(|(check, outcome)| {
                check.role == CheckRole::Receive && outcome.status == CheckStatus::Fail
            })
        {
            Verdict::NotReady
        } else if ctx
            .config
            .as_ref()
            .is_some_and(|cfg| cfg.station.callsign == "N0CALL" || !cfg.rig.interface.enabled)
            || results.iter().any(|(check, outcome)| {
                check.role == CheckRole::Transmit && outcome.status != CheckStatus::Pass
            })
        {
            Verdict::DecodeOnly
        } else {
            Verdict::OnAirReady
        };
        Self {
            verdict,
            exit_code: verdict.exit_code(),
            config_path: ctx.config_path.display().to_string(),
            checks: results
                .into_iter()
                .map(|(check, outcome)| CheckResult {
                    name: check.name,
                    status: outcome.status,
                    detail: outcome.detail,
                    fix: outcome.fix,
                })
                .collect(),
        }
    }

    pub fn collect(ctx: &DoctorCtx) -> Self {
        let results = build_checks()
            .into_iter()
            .map(|check| {
                let outcome = (check.run)(ctx);
                (check, outcome)
            })
            .collect();
        Self::from_results(ctx, results)
    }

    pub fn text(&self) -> String {
        use std::fmt::Write;
        let mut text = String::new();
        writeln!(
            text,
            "\npancetta doctor — {} checks (config: {})\n",
            self.checks.len(),
            self.config_path
        )
        .unwrap();
        for check in &self.checks {
            writeln!(
                text,
                "[{}] {:<20} {}",
                status_label(check.status),
                check.name,
                check.detail
            )
            .unwrap();
            if let Some(fix) = check.fix.as_deref().filter(|fix| !fix.is_empty()) {
                writeln!(text, "       fix: {fix}").unwrap();
            }
        }
        writeln!(text, "\n{}", self.verdict.result_line()).unwrap();
        text
    }
}

fn not_configured() -> CheckOutcome {
    CheckOutcome {
        status: CheckStatus::Warn,
        detail: "not configured — run the wizard".to_string(),
        fix: Some("run `pancetta` (first-run wizard) or `pancetta setup`".to_string()),
    }
}

pub(crate) fn check_config() -> DoctorCheck {
    DoctorCheck {
        name: "config",
        role: CheckRole::Receive,
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

/// Classify one SNTP result. PASS below FT8's ~0.3 s decode margin, WARN
/// from the margin up to 1 s (PAN-114), FAIL from 1 s, WARN when the server
/// cannot be reached. Only FAIL blocks receive readiness.
fn clock_outcome(result: Result<f64, String>) -> CheckOutcome {
    match result {
        Ok(offset) if offset.abs() < CLOCK_SKEW_WARN_S => CheckOutcome {
            status: CheckStatus::Pass,
            detail: format!("offset {offset:+.3} s vs {NTP_SERVER_HOST}"),
            fix: None,
        },
        Ok(offset) if offset.abs() < CLOCK_SKEW_FAIL_S => CheckOutcome {
            status: CheckStatus::Warn,
            detail: format!("offset {offset:+.2} s — over FT8's ~0.3 s decode margin"),
            fix: Some(clock_fix_hint().to_string()),
        },
        // FT8 slots are UTC-aligned; past ~1 s decodes fail systematically.
        Ok(offset) => CheckOutcome {
            status: CheckStatus::Fail,
            detail: format!("offset {offset:+.2} s — FT8 needs < ~1 s from UTC"),
            fix: Some(clock_fix_hint().to_string()),
        },
        Err(e) => CheckOutcome {
            status: CheckStatus::Warn,
            detail: format!("could not reach {NTP_SERVER_HOST} ({e}) — clock unverified"),
            fix: Some("check network; FT8 needs the clock within ~1 s of UTC".to_string()),
        },
    }
}

pub(crate) fn check_clock() -> DoctorCheck {
    DoctorCheck {
        name: "system clock",
        role: CheckRole::Receive,
        run: Box::new(|_ctx| {
            clock_outcome(sntp_clock_offset(NTP_SERVER, SNTP_TIMEOUT).map_err(|e| e.to_string()))
        }),
    }
}

pub(crate) fn check_audio_device() -> DoctorCheck {
    DoctorCheck {
        name: "audio input device",
        role: CheckRole::Receive,
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

fn audio_level_outcome(
    result: anyhow::Result<(f32, f32)>,
    authorization: MicrophoneAuthorization,
) -> CheckOutcome {
    match result {
        Ok((rms, peak)) => {
            let dbfs = 20.0 * rms.max(1e-9).log10();
            if peak < 1e-6 {
                let advice = match authorization {
                    MicrophoneAuthorization::Denied => Some((
                        "FLAT LINE: microphone access has not been approved on this Mac",
                        "open System Settings > Privacy & Security > Microphone; enable access for pancetta \
                        or the terminal app running it, then restart that app and re-run `pancetta doctor`. \
                        This approval is normally needed only once.",
                    )),
                    MicrophoneAuthorization::NotDetermined => Some((
                        "FLAT LINE: microphone access has not been approved on this Mac",
                        "allow microphone access when macOS asks for pancetta or the terminal app running it. \
                        If already listed, enable it in System Settings > Privacy & Security > Microphone; \
                        restart that app and re-run `pancetta doctor`. This approval is normally needed only \
                        once.",
                    )),
                    MicrophoneAuthorization::Restricted => Some((
                        "FLAT LINE: microphone access is restricted on this Mac",
                        "ask the Mac's administrator to allow microphone access for pancetta or the terminal \
                        app running it, then re-run `pancetta doctor`.",
                    )),
                    MicrophoneAuthorization::Authorized | MicrophoneAuthorization::Unknown => None,
                };
                if let Some((detail, fix)) = advice {
                    return CheckOutcome {
                        status: CheckStatus::Fail,
                        detail: detail.to_string(),
                        fix: Some(fix.to_string()),
                    };
                }
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
}

pub(crate) fn check_audio_level() -> DoctorCheck {
    DoctorCheck {
        name: "audio level (2 s)",
        role: CheckRole::Receive,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else {
                return not_configured();
            };
            let result = capture_rms(cfg, Duration::from_secs(2));
            // Read after capture so an approval changed during it is reflected.
            let authorization = match &result {
                Ok((_, peak)) if *peak < 1e-6 => microphone_authorization(),
                _ => MicrophoneAuthorization::Unknown,
            };
            audio_level_outcome(result, authorization)
        }),
    }
}

pub(crate) fn check_decoder() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib decoder",
        // Soft by invariant: the stub build is degraded, never fatal.
        role: CheckRole::Advisory,
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

/// Pure model-resolution half of the rigctld check (PAN-340): no process
/// spawn, no network, no global PATH read. `None` means the configured
/// model resolves; `Some(outcome)` is the failure to report, using the
/// same resolver message and numeric-ID guidance config validation shows
/// instead of directing operators only to the small supported-rigs page.
pub(crate) fn rig_model_outcome(model: &str) -> Option<CheckOutcome> {
    match pancetta_hamlib::resolve_model(model) {
        Ok(_) => None,
        Err(err) => Some(CheckOutcome {
            status: CheckStatus::Fail,
            detail: err.to_string(),
            fix: Some(
                "run `rigctl -l` to find your model, then set rig.model to its name or \
                 numeric ID (for example \"IC-7300\" or \"1045\"); see docs/SUPPORTED_RIGS.md"
                    .to_string(),
            ),
        }),
    }
}

pub(crate) fn check_rigctld() -> DoctorCheck {
    DoctorCheck {
        name: "rig / rigctld",
        role: CheckRole::Transmit,
        run: Box::new(|ctx| {
            rigctld_outcome(ctx, || {
                std::process::Command::new("rigctld")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok()
            })
        }),
    }
}

fn rigctld_outcome(ctx: &DoctorCtx, on_path: impl FnOnce() -> bool) -> CheckOutcome {
    let Some(cfg) = &ctx.config else {
        return not_configured();
    };
    if !cfg.rig.interface.enabled {
        return CheckOutcome {
            status: CheckStatus::Warn,
            detail: "rig disabled — decode-only (mock rig, no PTT)".to_string(),
            fix: Some("to go on-air: `pancetta setup` and enable rig control".to_string()),
        };
    }
    // Model must resolve or the coordinator can't spawn rigctld.
    // Use the shared resolver so names and numeric IDs match config validation.
    if let Some(outcome) = rig_model_outcome(&cfg.rig.model) {
        return outcome;
    }
    // Binary on PATH — pancetta spawns `rigctld` at startup (hamlib.rs:222).
    if !on_path() {
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
        .map(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok())
        .unwrap_or(false);
    let detail = if reachable {
        format!("rigctld on PATH; already listening on {host}:{port}")
    } else {
        format!("rigctld on PATH; nothing on {host}:{port} yet (pancetta spawns it at startup)")
    };
    CheckOutcome {
        status: CheckStatus::Pass,
        detail,
        fix: None,
    }
}

pub(crate) fn check_submodule() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib submodule",
        role: CheckRole::Advisory,
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

const MIB: u64 = 1024 * 1024;

fn log_directory() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta/logs")
}

fn existing_ancestor(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let mut current = path;
    loop {
        match std::fs::metadata(current) {
            Ok(_) => return Ok(current.to_path_buf()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent().ok_or(e)?;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn available_log_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let ancestor = existing_ancestor(path)?;
    let name = std::ffi::CString::new(ancestor.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: name is NUL-terminated and stats is writable for the entire syscall.
    if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statvfs initialized stats on success.
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

fn disk_outcome(path: &std::path::Path, result: std::io::Result<u64>) -> CheckOutcome {
    match result {
        Ok(bytes) => {
            let status = if bytes < 10 * MIB {
                CheckStatus::Fail
            } else if bytes < 100 * MIB {
                CheckStatus::Warn
            } else {
                CheckStatus::Pass
            };
            CheckOutcome {
                status,
                detail: format!("{}: {} MiB available", path.display(), bytes / MIB),
                fix: (status != CheckStatus::Pass).then(|| {
                    format!(
                        "free space on the volume containing {}; keep qsos.adi and qso.db",
                        path.display()
                    )
                }),
            }
        }
        Err(e) => CheckOutcome {
            status: CheckStatus::Warn,
            detail: format!("could not inspect {}: {e}", path.display()),
            fix: Some(format!(
                "check access to {} and inspect its volume with the OS disk tool",
                path.display()
            )),
        },
    }
}

fn check_disk() -> DoctorCheck {
    DoctorCheck {
        name: "log disk space",
        role: CheckRole::Receive,
        run: Box::new(|_| {
            #[cfg(unix)]
            {
                let path = log_directory();
                disk_outcome(&path, available_log_bytes(&path))
            }
            #[cfg(not(unix))]
            {
                CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "disk-space query unavailable on this OS".into(),
                    fix: Some(
                        "inspect the volume containing the station logs with the OS disk tool"
                            .into(),
                    ),
                }
            }
        }),
    }
}

fn permission_outcome(is_macos: bool, state: MicrophoneAuthorization) -> CheckOutcome {
    use MicrophoneAuthorization::*;
    if !is_macos {
        return CheckOutcome {
            status: CheckStatus::Pass,
            detail: "not applicable on this OS; audio capture is checked separately".into(),
            fix: None,
        };
    }
    let (status, detail, fix) = match state {
        Authorized => (CheckStatus::Pass, "access authorized on this Mac", None),
        Denied => (CheckStatus::Fail, "access denied on this Mac", Some("open System Settings > Privacy & Security > Microphone; enable pancetta or its terminal app, restart that app and re-run `pancetta doctor`")),
        Restricted => (CheckStatus::Fail, "access restricted on this Mac", Some("ask the Mac's administrator to allow microphone access for pancetta or its terminal app")),
        NotDetermined => (CheckStatus::Warn, "approval not yet requested on this Mac", Some("run `pancetta test-audio` locally at the Mac and allow microphone access, then re-run `pancetta doctor`")),
        Unknown => (CheckStatus::Warn, "authorization could not be queried on this Mac", Some("run `pancetta test-audio` locally and check System Settings > Privacy & Security > Microphone")),
    };
    CheckOutcome {
        status,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

fn check_microphone() -> DoctorCheck {
    DoctorCheck {
        name: "microphone permission",
        role: CheckRole::Receive,
        run: Box::new(|_| {
            permission_outcome(cfg!(target_os = "macos"), microphone_authorization())
        }),
    }
}

type Reachability = (String, Result<(), String>);

fn network_outcome(results: Vec<Reachability>) -> CheckOutcome {
    if results.is_empty() {
        return CheckOutcome {
            status: CheckStatus::Warn,
            detail: "no network services enabled".into(),
            fix: Some(
                "enable [network.cqdx] or [network.dx_cluster] if online services are wanted"
                    .into(),
            ),
        };
    }
    let failed: Vec<_> = results
        .into_iter()
        .filter_map(|(endpoint, result)| result.err().map(|reason| (endpoint, reason)))
        .collect();
    if failed.is_empty() {
        return CheckOutcome {
            status: CheckStatus::Pass,
            detail: "all enabled services reachable".into(),
            fix: None,
        };
    }
    CheckOutcome {
        status: CheckStatus::Warn,
        detail: failed
            .iter()
            .map(|(endpoint, reason)| format!("{endpoint} unreachable ({reason})"))
            .collect::<Vec<_>>()
            .join("; "),
        fix: Some(format!(
            "check DNS, routing and firewall access to {}",
            failed
                .iter()
                .map(|(endpoint, _)| endpoint.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "DNS, connection or TLS failed"
    } else {
        "HTTP transport failed"
    }
    .into()
}

async fn cqdx_probe(base: String) -> Reachability {
    let Ok(mut url) = reqwest::Url::parse(&base) else {
        return ("cqdx endpoint".into(), Err("invalid URL".into()));
    };
    let endpoint = format!(
        "{}:{}",
        url.host_str().unwrap_or("invalid host"),
        url.port_or_known_default().unwrap_or(0)
    );
    if !matches!(url.scheme(), "http" | "https") {
        return (endpoint, Err("unsupported URL scheme".into()));
    }
    // This public endpoint needs no credentials, path/query secrets or redirects.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&format!(
        "{}/api/v1/entities",
        url.path().trim_end_matches('/')
    ));
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return (endpoint, Err("HTTP client unavailable".into())),
    };
    // A 401 also proves reachability: the endpoint answered and doctor sends
    // no credentials to take that answer further (PAN-406).
    let result = match client.get(url).send().await {
        Ok(response) if response.status().is_success() || response.status().as_u16() == 401 => {
            Ok(())
        }
        Ok(response) => Err(format!("HTTP {}", response.status().as_u16())),
        Err(error) => Err(transport_reason(&error)),
    };
    (endpoint, result)
}

async fn cluster_probe(servers: Vec<(String, u16)>) -> Reachability {
    if servers.is_empty() {
        return ("DX cluster".into(), Err("no servers configured".into()));
    }
    let mut attempts = tokio::task::JoinSet::new();
    for (host, port) in servers {
        attempts.spawn(async move {
            let endpoint = format!("{}:{port}", safe_host(&host));
            let result: Result<(), String> = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map(|_| ())
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::ConnectionRefused => "connection refused".into(),
                    std::io::ErrorKind::TimedOut => "timed out".into(),
                    _ => "DNS or connection failed".into(),
                });
            (endpoint, result)
        });
    }
    let mut failures = Vec::new();
    while let Some(Ok((endpoint, result))) = attempts.join_next().await {
        if result.is_ok() {
            return ("DX cluster".into(), Ok(()));
        }
        if failures.len() < 3 {
            failures.push(format!("{endpoint}: {}", result.unwrap_err()));
        }
    }
    ("DX cluster".into(), Err(failures.join(", ")))
}

fn safe_host(host: &str) -> String {
    host.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        .take(253)
        .collect()
}

async fn network_probe(cfg: Config, deadline: Duration) -> Vec<Reachability> {
    let mut probes = tokio::task::JoinSet::new();
    let mut pending = Vec::new();
    if cfg.network.cqdx.enabled {
        let base = cfg.network.cqdx.base_url;
        let label = reqwest::Url::parse(&base)
            .ok()
            .map(|u| {
                format!(
                    "{}:{}",
                    u.host_str().unwrap_or("invalid host"),
                    u.port_or_known_default().unwrap_or(0)
                )
            })
            .unwrap_or_else(|| "cqdx endpoint".into());
        pending.push(label);
        probes.spawn(cqdx_probe(base));
    }
    if cfg.network.dx_cluster.enabled {
        pending.push("DX cluster".into());
        probes.spawn(cluster_probe(
            cfg.network
                .dx_cluster
                .servers
                .into_iter()
                .map(|s| (s.hostname, s.port))
                .collect(),
        ));
    }
    collect_network_probes(probes, pending, deadline).await
}

// All enabled integrations share this deadline, including blocking resolver
// work awaited by their asynchronous connect/send futures.
async fn collect_network_probes(
    mut probes: tokio::task::JoinSet<Reachability>,
    mut pending: Vec<String>,
    deadline: Duration,
) -> Vec<Reachability> {
    let mut results = Vec::new();
    let end = tokio::time::Instant::now() + deadline;
    while !pending.is_empty() {
        match tokio::time::timeout_at(end, probes.join_next()).await {
            Ok(Some(Ok((label, result)))) => {
                if let Some(index) = pending.iter().position(|p| *p == label) {
                    pending.remove(index);
                }
                results.push((label, result));
            }
            _ => {
                results.extend(
                    pending
                        .drain(..)
                        .map(|label| (label, Err("timed out".into()))),
                );
                break;
            }
        }
    }
    results
}

fn check_network() -> DoctorCheck {
    DoctorCheck {
        name: "network services",
        role: CheckRole::Advisory,
        run: Box::new(|ctx| {
            let Some(cfg) = ctx.config.clone() else {
                return not_configured();
            };
            // Doctor runs inside async main. Own a separate runtime and abandon any
            // resolver's blocking work after the shared deadline instead of joining it.
            match std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| ())?;
                let result = runtime.block_on(network_probe(cfg, Duration::from_secs(3)));
                runtime.shutdown_background();
                Ok::<_, ()>(network_outcome(result))
            })
            .join()
            {
                Ok(Ok(outcome)) => outcome,
                _ => network_outcome(vec![(
                    "network probe".into(),
                    Err("worker unavailable".into()),
                )]),
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn http_fixture(status: u16, stall: bool) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with("GET /api/v1/entities "));
            assert!(!request.to_lowercase().contains("authorization:"));
            if stall {
                std::future::pending::<()>().await;
            }
            stream
                .write_all(
                    format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
        });
        (url, task)
    }

    #[tokio::test]
    async fn network_probe_matrix() {
        for (status, expected) in [
            (200, CheckStatus::Pass),
            // A 401 proves the endpoint answered; doctor sends no credentials.
            (401, CheckStatus::Pass),
            (302, CheckStatus::Warn),
            (503, CheckStatus::Warn),
        ] {
            let (url, task) = http_fixture(status, false).await;
            let mut cfg = Config::default();
            cfg.network.cqdx.enabled = true;
            cfg.network.cqdx.base_url =
                url.replace("http://", "http://secret:password@") + "?token=secret";
            cfg.network.cqdx.token = Some("secret".into());
            let outcome = network_outcome(network_probe(cfg, Duration::from_secs(1)).await);
            assert_eq!(outcome.status, expected);
            assert!(!outcome.detail.contains("secret"));
            task.await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(cluster_probe(vec![("127.0.0.1".into(), port)])
            .await
            .1
            .is_ok());
        let refused_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_port = refused_listener.local_addr().unwrap().port();
        drop(refused_listener);
        assert!(cluster_probe(vec![
            ("127.0.0.1".into(), refused_port),
            ("127.0.0.1".into(), port)
        ])
        .await
        .1
        .is_ok());
        drop(listener);
        let refused = cluster_probe(vec![("127.0.0.1".into(), port)]).await;
        assert!(refused.1.unwrap_err().contains("connection refused"));
        // Invalid DNS/transport results are advisory and retain no raw URL errors.
        let outcome = network_outcome(vec![(
            "invalid.example:443".into(),
            Err("DNS or connection failed".into()),
        )]);
        assert_eq!(outcome.status, CheckStatus::Warn);
        let disabled = network_probe(Config::default(), Duration::from_secs(1)).await;
        assert!(disabled.is_empty());
        assert_eq!(
            network_outcome(disabled).detail,
            "no network services enabled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn network_probe_deadline() {
        let mut probes = tokio::task::JoinSet::new();
        // Simulate a resolver that never completes. This future follows the same
        // collector path as reqwest/Tokio DNS, with no public DNS dependency.
        probes.spawn(async {
            std::future::pending::<()>().await;
            ("dns.example:443".into(), Ok(()))
        });
        probes.spawn(async {
            (
                "refused.example:7300".into(),
                Err("connection refused".into()),
            )
        });
        let start = tokio::time::Instant::now();
        let result = collect_network_probes(
            probes,
            vec!["dns.example:443".into(), "refused.example:7300".into()],
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
        assert_eq!(result.len(), 2);
        assert!(result
            .iter()
            .any(|(endpoint, outcome)| endpoint == "dns.example:443"
                && outcome.as_ref().map_err(String::as_str) == Err("timed out")));
        assert!(result
            .iter()
            .any(|(endpoint, outcome)| endpoint == "refused.example:7300"
                && outcome.as_ref().map_err(String::as_str) == Err("connection refused")));
        assert_eq!(network_outcome(result).status, CheckStatus::Warn);
    }

    #[test]
    fn disk_space_thresholds() {
        let p = std::path::Path::new("/logs");
        for (bytes, status) in [
            (0, CheckStatus::Fail),
            (10 * MIB - 1, CheckStatus::Fail),
            (10 * MIB, CheckStatus::Warn),
            (100 * MIB - 1, CheckStatus::Warn),
            (100 * MIB, CheckStatus::Pass),
        ] {
            assert_eq!(disk_outcome(p, Ok(bytes)).status, status);
        }
    }

    #[test]
    fn disk_ancestor_and_error_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-created/logs");
        assert_eq!(existing_ancestor(&missing).unwrap(), dir.path());
        assert!(!missing.exists());
        let outcome = disk_outcome(
            &missing,
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        );
        assert_eq!(outcome.status, CheckStatus::Warn);
        assert!(outcome.detail.contains("could not inspect"));
        assert!(!outcome.detail.contains("0 MiB"));
    }

    #[cfg(unix)]
    #[test]
    fn disk_probe_reads_temp_volume() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_log_bytes(dir.path()).unwrap() > 0);
        assert!(available_log_bytes(&dir.path().join("missing/logs")).unwrap() > 0);
        assert_eq!(
            existing_ancestor(&dir.path().join("missing/logs")).unwrap(),
            dir.path()
        );
    }

    #[test]
    fn microphone_permission_matrix() {
        use MicrophoneAuthorization::*;
        for (state, status, text) in [
            (Authorized, CheckStatus::Pass, "access authorized"),
            (Denied, CheckStatus::Fail, "access denied"),
            (Restricted, CheckStatus::Fail, "access restricted"),
            (
                NotDetermined,
                CheckStatus::Warn,
                "approval not yet requested",
            ),
            (
                Unknown,
                CheckStatus::Warn,
                "authorization could not be queried",
            ),
        ] {
            let outcome = permission_outcome(true, state);
            assert_eq!(outcome.status, status);
            assert!(outcome.detail.starts_with(text));
            assert_eq!(outcome.fix.is_none(), status == CheckStatus::Pass);
        }
        assert_eq!(permission_outcome(false, Denied).status, CheckStatus::Pass);
    }

    #[test]
    fn permission_is_independent_of_capture() {
        for result in [Ok((0.01, 0.02)), Err(anyhow::anyhow!("capture failed"))] {
            let _ = audio_level_outcome(result, MicrophoneAuthorization::Denied);
            assert_eq!(
                permission_outcome(true, MicrophoneAuthorization::Denied).status,
                CheckStatus::Fail
            );
        }
    }

    #[test]
    fn new_checks_gate_only_hard_failures() {
        let checks = build_checks();
        assert_eq!(checks.len(), 10);
        assert_eq!(checks[7].name, "network services");
        assert_eq!(checks[8].name, "log disk space");
        assert_eq!(checks[9].name, "microphone permission");
        // Network is advisory; disk and microphone are receive prerequisites.
        assert!(checks[7].role == CheckRole::Advisory);
        assert!(checks[8].role == CheckRole::Receive);
        assert!(checks[9].role == CheckRole::Receive);
        let ctx = fixture_ctx();
        let mut statuses = [CheckStatus::Pass; 10];
        statuses[7] = CheckStatus::Fail;
        assert_eq!(
            fixture_report(&ctx, statuses).verdict,
            Verdict::OnAirReady,
            "a network failure stays advisory"
        );
        for index in [8, 9] {
            let mut statuses = [CheckStatus::Pass; 10];
            statuses[index] = CheckStatus::Fail;
            let report = fixture_report(&ctx, statuses);
            assert_eq!(report.verdict, Verdict::NotReady, "check {index} is hard");
            assert_eq!(report.exit_code, 3);
        }
    }

    #[test]
    fn field_failures_render_together() {
        let ctx = build_ctx(Some(PathBuf::from("/nonexistent/pancetta.toml")));
        let mut checks = build_checks();
        checks[7].run =
            Box::new(|_| network_outcome(vec![("cqdx.io:443".into(), Err("timed out".into()))]));
        checks[8].run = Box::new(|_| disk_outcome(std::path::Path::new("/logs"), Ok(0)));
        checks[9].run = Box::new(|_| permission_outcome(true, MicrophoneAuthorization::Denied));
        let results: Vec<_> = checks
            .into_iter()
            .map(|check| {
                let outcome = (check.run)(&ctx);
                (check, outcome)
            })
            .collect();
        let report = DoctorReport::from_results(&ctx, results);
        let text = report.text();
        for label in [
            "network services",
            "log disk space",
            "microphone permission",
        ] {
            assert!(text.contains(label));
        }
        assert!(text.contains("fix: check DNS, routing and firewall access to cqdx.io:443"));
        assert!(text
            .contains("fix: free space on the volume containing /logs; keep qsos.adi and qso.db"));
        assert!(text.contains("fix: open System Settings > Privacy & Security > Microphone"));
        let fresh = build_checks();
        assert!(!(fresh[8].run)(&ctx).detail.contains("not configured"));
        assert!(!(fresh[9].run)(&ctx).detail.contains("not configured"));
    }

    fn fixture_ctx() -> DoctorCtx {
        let mut config = Config::default();
        config.station.callsign = "K1ABC".into();
        config.rig.interface.enabled = true;
        DoctorCtx {
            config_path: "station.toml".into(),
            config: Some(config),
            config_error: None,
        }
    }

    fn fixture_report(ctx: &DoctorCtx, statuses: [CheckStatus; 10]) -> DoctorReport {
        let checks = build_checks()
            .into_iter()
            .zip(statuses)
            .map(|(check, status)| {
                (
                    check,
                    CheckOutcome {
                        status,
                        detail: "fixture detail".into(),
                        fix: Some("fixture fix".into()),
                    },
                )
            })
            .collect();
        DoctorReport::from_results(ctx, checks)
    }

    fn fixture_report_with_audio(audio: CheckOutcome) -> DoctorReport {
        let ctx = fixture_ctx();
        let mut results: Vec<_> = build_checks()
            .into_iter()
            .map(|check| {
                (
                    check,
                    CheckOutcome {
                        status: CheckStatus::Pass,
                        detail: "ok".into(),
                        fix: None,
                    },
                )
            })
            .collect();
        results[3].1 = audio;
        DoctorReport::from_results(&ctx, results)
    }

    #[test]
    fn verdict_precedence() {
        use CheckStatus::*;
        let ctx = fixture_ctx();
        let cases = [
            ([Pass; 10], Verdict::OnAirReady),
            (
                [Pass, Warn, Warn, Pass, Fail, Pass, Fail, Pass, Pass, Pass],
                Verdict::OnAirReady,
            ),
            (
                [Pass, Fail, Pass, Pass, Pass, Fail, Pass, Pass, Pass, Pass],
                Verdict::NotReady,
            ),
            (
                [Fail, Pass, Pass, Pass, Pass, Fail, Pass, Pass, Pass, Pass],
                Verdict::NotReady,
            ),
            (
                [Pass, Pass, Fail, Pass, Pass, Fail, Pass, Pass, Pass, Pass],
                Verdict::NotReady,
            ),
            (
                [Pass, Pass, Pass, Fail, Pass, Fail, Pass, Pass, Pass, Pass],
                Verdict::NotReady,
            ),
            (
                [Pass, Pass, Pass, Pass, Pass, Fail, Pass, Pass, Pass, Pass],
                Verdict::DecodeOnly,
            ),
            // PAN-335: network is advisory, disk and microphone are receive
            // prerequisites.
            (
                [Pass, Pass, Pass, Pass, Pass, Pass, Pass, Fail, Pass, Pass],
                Verdict::OnAirReady,
            ),
            (
                [Pass, Pass, Pass, Pass, Pass, Pass, Pass, Pass, Fail, Pass],
                Verdict::NotReady,
            ),
            (
                [Pass, Pass, Pass, Pass, Pass, Pass, Pass, Pass, Pass, Fail],
                Verdict::NotReady,
            ),
        ];
        for (statuses, expected) in cases {
            assert_eq!(fixture_report(&ctx, statuses).verdict, expected);
        }
        let missing = build_ctx(Some("/nonexistent/pan338.toml".into()));
        assert_eq!(
            fixture_report(&missing, [Fail; 10]).verdict,
            Verdict::NotConfigured
        );
        let mut invalid = fixture_ctx();
        invalid.config_error = Some("invalid".into());
        assert_eq!(
            fixture_report(&invalid, [Pass; 10]).verdict,
            Verdict::NotReady
        );
    }

    #[test]
    fn decode_only_prerequisites() {
        let mut ctx = fixture_ctx();
        ctx.config.as_mut().unwrap().rig.interface.enabled = false;
        assert_eq!(
            fixture_report(&ctx, [CheckStatus::Pass; 10]).verdict,
            Verdict::DecodeOnly
        );
        ctx.config.as_mut().unwrap().rig.interface.enabled = true;
        ctx.config.as_mut().unwrap().station.callsign = "N0CALL".into();
        assert_eq!(
            fixture_report(&ctx, [CheckStatus::Pass; 10]).verdict,
            Verdict::DecodeOnly
        );
    }

    #[test]
    fn rig_outcomes_keep_fixes() {
        for (model, enabled, expected_fix) in [
            (
                "FTdx10",
                false,
                "to go on-air: `pancetta setup` and enable rig control",
            ),
            ("FTdx10", true, hamlib_install_hint()),
            ("not-a-rig", true, "docs/SUPPORTED_RIGS.md"),
        ] {
            let mut ctx = fixture_ctx();
            let cfg = ctx.config.as_mut().unwrap();
            cfg.rig.model = model.into();
            cfg.rig.interface.enabled = enabled;
            let rig = rigctld_outcome(&ctx, || false);
            assert!(rig.fix.as_ref().unwrap().contains(expected_fix));
            assert_eq!(
                rig.status,
                if enabled {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Warn
                }
            );
            let mut results: Vec<_> = build_checks()
                .into_iter()
                .map(|check| {
                    (
                        check,
                        CheckOutcome {
                            status: CheckStatus::Pass,
                            detail: "ok".into(),
                            fix: None,
                        },
                    )
                })
                .collect();
            results[5].1 = rig;
            let report = DoctorReport::from_results(&ctx, results);
            assert_eq!(report.verdict, Verdict::DecodeOnly);
            let json = serde_json::to_value(&report).unwrap();
            assert!(json["checks"][5]["fix"]
                .as_str()
                .unwrap()
                .contains(expected_fix));
            assert!(report.text().contains(expected_fix));
        }
    }

    #[test]
    fn report_renderings_agree() {
        use CheckStatus::*;
        let ctx = fixture_ctx();
        let missing = build_ctx(Some("/nonexistent/pan338.toml".into()));
        let fixtures = [
            (fixture_report(&missing, [Pass; 10]), "NOT CONFIGURED", 2,
                "Result: NOT CONFIGURED. Run `pancetta setup`, then re-run `pancetta doctor`."),
            (fixture_report(&ctx, [Pass, Pass, Pass, Pass, Pass, Fail, Pass, Pass, Pass, Pass]), "DECODE-ONLY", 1,
                "Result: DECODE-ONLY. Receive checks passed; fix the transmit prerequisites above to go on-air."),
            (fixture_report(&ctx, [Pass; 10]), "ON-AIR READY", 0,
                "Result: ON-AIR READY. Doctor prerequisites passed; PTT and transmit audio are not tested."),
            (fixture_report(&ctx, [Pass, Pass, Pass, Fail, Pass, Fail, Pass, Pass, Pass, Pass]), "NOT READY", 3,
                "Result: NOT READY. Fix the failed receive or configuration checks above, then re-run `pancetta doctor`."),
        ];
        let names = [
            "config",
            "system clock",
            "audio input device",
            "audio level (2 s)",
            "ft8_lib decoder",
            "rig / rigctld",
            "ft8_lib submodule",
            "network services",
            "log disk space",
            "microphone permission",
        ];
        for (report, label, code, result_line) in fixtures {
            let text = report.text();
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(report.exit_code, code);
            assert_eq!(json["exit_code"], code);
            assert_eq!(json["verdict"], label);
            assert!(text.ends_with(&format!("{result_line}\n")));
            for (index, name) in names.into_iter().enumerate() {
                assert_eq!(json["checks"][index]["name"], name);
                assert_eq!(json["checks"][index]["detail"], "fixture detail");
                assert_eq!(json["checks"][index]["fix"], "fixture fix");
                assert_eq!(
                    json["checks"][index]["status"],
                    status_label(report.checks[index].status)
                );
                assert!(text.contains(&format!(
                    "[{}] {:<20} fixture detail\n       fix: fixture fix",
                    status_label(report.checks[index].status),
                    name
                )));
            }
        }
        let mut report = fixture_report(&ctx, [Pass; 10]);
        report.checks[0].fix = None;
        assert!(serde_json::to_value(&report).unwrap()["checks"][0]["fix"].is_null());
    }

    #[test]
    fn silent_audio_names_missing_approval() {
        for state in [
            MicrophoneAuthorization::Denied,
            MicrophoneAuthorization::NotDetermined,
        ] {
            let outcome = audio_level_outcome(Ok((0.0, 0.0)), state);
            assert_eq!(outcome.status, CheckStatus::Fail);
            assert_eq!(
                outcome.detail,
                "FLAT LINE: microphone access has not been approved on this Mac"
            );
            let fix = outcome.fix.as_ref().unwrap();
            assert!(fix.contains("System Settings > Privacy & Security > Microphone"));
            assert!(fix.contains("pancetta or the terminal app running it"));
            assert!(fix.contains("restart that app"));
            assert!(fix.contains("normally needed only once"));
            assert_eq!(fixture_report_with_audio(outcome).exit_code, 3);
        }
        let denied = audio_level_outcome(Ok((0.0, 0.0)), MicrophoneAuthorization::Denied);
        assert!(denied.fix.unwrap().starts_with("open System Settings"));
        let undecided = audio_level_outcome(Ok((0.0, 0.0)), MicrophoneAuthorization::NotDetermined);
        assert!(undecided
            .fix
            .unwrap()
            .starts_with("allow microphone access when macOS asks"));
    }

    #[test]
    fn restricted_audio_refers_to_administrator() {
        let outcome = audio_level_outcome(Ok((0.0, 0.0)), MicrophoneAuthorization::Restricted);
        assert_eq!(outcome.status, CheckStatus::Fail);
        assert_eq!(
            outcome.detail,
            "FLAT LINE: microphone access is restricted on this Mac"
        );
        assert_eq!(outcome.fix.as_deref(), Some("ask the Mac's administrator to allow microphone access for pancetta or the terminal app running it, then re-run `pancetta doctor`."));
    }

    #[test]
    fn authorized_and_unknown_silence_keep_generic_advice() {
        for state in [
            MicrophoneAuthorization::Authorized,
            MicrophoneAuthorization::Unknown,
        ] {
            let outcome = audio_level_outcome(Ok((0.0, 0.0)), state);
            assert_eq!(outcome.status, CheckStatus::Fail);
            assert_eq!(
                outcome.detail,
                "FLAT LINE — no signal reaching the input at all"
            );
            assert_eq!(outcome.fix.as_deref(), Some("wrong input device, OS-muted, or rig AF off — `pancetta test-audio --list`, check OS input level, turn up the rig's USB audio out"));
            for text in [&outcome.detail, outcome.fix.as_ref().unwrap()] {
                assert!(!text.contains("permission"));
                assert!(!text.contains("microphone"));
                assert!(!text.contains("approved"));
            }
        }
    }

    #[test]
    fn audio_threshold_and_signal_ignore_denied_authorization() {
        let state = MicrophoneAuthorization::Denied;
        assert_eq!(
            audio_level_outcome(Ok((0.0, 0.999e-6)), state).status,
            CheckStatus::Fail
        );
        assert_eq!(
            audio_level_outcome(Ok((0.0, 1e-6)), state).status,
            CheckStatus::Pass
        );
        let signal = audio_level_outcome(Ok((0.01, 0.02)), state);
        assert_eq!(signal.status, CheckStatus::Pass);
        assert_eq!(signal.detail, "rms -40 dBFS, peak 0.020");
        assert!(signal.fix.is_none());
    }

    #[test]
    fn audio_capture_errors_are_not_flat_line() {
        for error in ["no samples captured in 2s", "device unavailable"] {
            let outcome =
                audio_level_outcome(Err(anyhow::anyhow!(error)), MicrophoneAuthorization::Denied);
            assert_eq!(outcome.status, CheckStatus::Fail);
            assert_eq!(outcome.detail, format!("could not capture: {error}"));
            assert_eq!(outcome.fix.as_deref(), Some("run `pancetta test-audio --list`; close other apps holding the device (including a running pancetta)"));
        }
    }

    #[test]
    fn audio_without_config_keeps_wizard_advice() {
        let ctx = build_ctx(Some(PathBuf::from("/nonexistent/dir/pancetta.toml")));
        let check = check_audio_level();
        assert_eq!(check.name, "audio level (2 s)");
        assert!(check.role == CheckRole::Receive);
        let outcome = (check.run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Warn);
        assert_eq!(outcome.detail, "not configured — run the wizard");
        assert_eq!(
            outcome.fix.as_deref(),
            Some("run `pancetta` (first-run wizard) or `pancetta setup`")
        );
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
        assert!(check_config().role == CheckRole::Receive);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clock_outcome_passes_inside_the_ft8_margin() {
        let o = clock_outcome(Ok(0.12));
        assert_eq!(o.status, CheckStatus::Pass);
        assert_eq!(o.detail, "offset +0.120 s vs pool.ntp.org");
        assert!(o.fix.is_none());
    }

    #[test]
    fn clock_outcome_warns_between_margin_and_one_second() {
        let o = clock_outcome(Ok(0.45));
        assert_eq!(o.status, CheckStatus::Warn);
        assert_eq!(o.detail, "offset +0.45 s — over FT8's ~0.3 s decode margin");
        assert_eq!(o.fix.as_deref(), Some(clock_fix_hint()));
        // The margin itself is inclusive, matching the live monitor.
        assert_eq!(clock_outcome(Ok(-0.3)).status, CheckStatus::Warn);
        assert_eq!(clock_outcome(Ok(0.299)).status, CheckStatus::Pass);
    }

    #[test]
    fn clock_outcome_fails_at_one_second_unchanged() {
        let o = clock_outcome(Ok(-1.2));
        assert_eq!(o.status, CheckStatus::Fail);
        assert_eq!(o.detail, "offset -1.20 s — FT8 needs < ~1 s from UTC");
        assert_eq!(o.fix.as_deref(), Some(clock_fix_hint()));
        assert_eq!(clock_outcome(Ok(1.0)).status, CheckStatus::Fail);
        assert!(check_clock().role == CheckRole::Receive);
    }

    #[test]
    fn clock_outcome_unreachable_is_a_warn_unchanged() {
        let o = clock_outcome(Err("timed out".to_string()));
        assert_eq!(o.status, CheckStatus::Warn);
        assert_eq!(
            o.detail,
            "could not reach pool.ntp.org (timed out) — clock unverified"
        );
        assert_eq!(
            o.fix.as_deref(),
            Some("check network; FT8 needs the clock within ~1 s of UTC")
        );
    }

    /// PAN-340: the doctor's model check resolves through the shared offline
    /// resolver — numeric IDs and catalog names pass, errors reuse the
    /// resolver's wording with numeric-ID guidance — without spawning
    /// rigctld or touching global PATH.
    #[test]
    fn model_diagnostic_uses_shared_resolver() {
        // Numeric and catalog names (including non-curated ones) resolve.
        for model in [
            "1045",
            "IC-7300",
            "KX3",
            "Barrett 950",
            "  ft-dx10  ",
            "123456",
        ] {
            assert!(
                rig_model_outcome(model).is_none(),
                "{model:?} must resolve for doctor"
            );
        }
        // Unknown name: the resolver body, with matches and the hint.
        let outcome = rig_model_outcome("FT-9999").expect("FT-9999 must fail");
        assert_eq!(outcome.status, CheckStatus::Fail);
        assert!(
            outcome.detail.starts_with(
                "Unknown rig.model 'FT-9999'. Closest matches: FT-990 (1016), \
                 FT-991 (1035), FT-890 (1015)."
            ),
            "{}",
            outcome.detail
        );
        let fix = outcome.fix.expect("unknown model needs a fix");
        assert!(fix.contains("rigctl -l"), "{fix}");
        assert!(fix.contains("numeric ID"), "{fix}");
        // Malformed numeric value: the numeric-id error body.
        let outcome = rig_model_outcome("0").expect("'0' must fail");
        assert!(
            outcome.detail.starts_with("Invalid rig.model '0'."),
            "{}",
            outcome.detail
        );
        // Ambiguity names every backend.
        let outcome = rig_model_outcome("SW/FM radio").expect("ambiguity must fail");
        assert!(
            outcome
                .detail
                .starts_with("Ambiguous rig.model 'SW/FM radio'"),
            "{}",
            outcome.detail
        );
    }
}
