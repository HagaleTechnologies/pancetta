//! `pancetta doctor` — fast, independent station-health checks, each with a
//! one-line fix. The universal first answer to "it doesn't work" and the
//! first line of every troubleshooting doc. Checks are data
//! ([`DoctorCheck`] entries in [`build_checks`]) so adding one is a
//! one-liner for later phases.

use pancetta_config::Config;
use std::path::PathBuf;

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
        check_audio_device(),
        check_decoder(),
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
}
