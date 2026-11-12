//! `pancetta doctor` — fast, independent station-health checks, each with a
//! one-line fix. The universal first answer to "it doesn't work" and the
//! first line of every troubleshooting doc. Checks are data
//! ([`DoctorCheck`] entries in [`build_checks`]) so adding one is a
//! one-liner for later phases.

use pancetta_config::Config;
use pancetta_lib::clock_skew::{
    clock_fix_hint, sntp_clock_offset, CLOCK_SKEW_FAIL_S, CLOCK_SKEW_WARN_S, NTP_SERVER,
    NTP_SERVER_HOST, SNTP_TIMEOUT,
};
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

/// Classify one SNTP result. PASS below FT8's ~0.3 s decode margin, WARN
/// from the margin up to 1 s (PAN-114), FAIL from 1 s, WARN when the server
/// cannot be reached. Only FAIL gates the exit code (`check_clock` is hard).
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
        hard: true,
        run: Box::new(|_ctx| {
            clock_outcome(sntp_clock_offset(NTP_SERVER, SNTP_TIMEOUT).map_err(|e| e.to_string()))
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

// Known authorization is diagnostic evidence only when captured audio is silent.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum MicrophoneAuthorization {
    Authorized,
    Denied,
    NotDetermined,
    Restricted,
    Unknown,
}

/// Read the current process's authorization without requesting access or changing it.
#[cfg(target_os = "macos")]
fn microphone_authorization() -> MicrophoneAuthorization {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    // SAFETY: AVMediaTypeAudio is an immutable framework constant. This query
    // accepts the audio media type, needs no device instance or main thread,
    // and only reads authorization for the calling process.
    let status = unsafe {
        let Some(audio) = AVMediaTypeAudio else {
            return MicrophoneAuthorization::Unknown;
        };
        AVCaptureDevice::authorizationStatusForMediaType(audio)
    };
    match status {
        AVAuthorizationStatus::Authorized => MicrophoneAuthorization::Authorized,
        AVAuthorizationStatus::Denied => MicrophoneAuthorization::Denied,
        AVAuthorizationStatus::NotDetermined => MicrophoneAuthorization::NotDetermined,
        AVAuthorizationStatus::Restricted => MicrophoneAuthorization::Restricted,
        _ => MicrophoneAuthorization::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
fn microphone_authorization() -> MicrophoneAuthorization {
    MicrophoneAuthorization::Unknown
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
        hard: true,
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
            // Model must resolve or the coordinator can't spawn rigctld
            // (hamlib.rs warns and gives up). Pure half of this check,
            // factored out so it stays testable without spawning anything.
            if let Some(outcome) = rig_model_outcome(&cfg.rig.model) {
                return outcome;
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
            let fix = outcome.fix.unwrap();
            assert!(fix.contains("System Settings > Privacy & Security > Microphone"));
            assert!(fix.contains("pancetta or the terminal app running it"));
            assert!(fix.contains("restart that app"));
            assert!(fix.contains("normally needed only once"));
            assert_eq!(
                doctor_exit_code(&[(check_audio_level().hard, outcome.status)]),
                1
            );
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
        assert!(check.hard);
        let outcome = (check.run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Warn);
        assert_eq!(outcome.detail, "not configured — run the wizard");
        assert_eq!(
            outcome.fix.as_deref(),
            Some("run `pancetta` (first-run wizard) or `pancetta setup`")
        );
    }

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
        assert!(check_clock().hard);
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
