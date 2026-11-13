//! PAN-90: `pancetta config --validate` and startup against the real binary.
//!
//! Every documented minimal recipe must pass validation on its own, and a
//! config file that exists but fails to load must fail validation (and
//! startup) with a non-zero exit naming the file -- never silently fall back
//! to an all-defaults (N0CALL) config while printing PASS.
//!
//! PAN-301: that contract runs deeper than "exit non-zero". A malformed
//! discovered file stops plain `--headless` startup before station
//! initialization, naming the TOML source line; interactive startup offers
//! the de-brick wizard (declining leaves the file untouched); unattended
//! non-terminal startup never prompts; and `config --validate` never offers
//! repair even with a terminal on stdin.
//!
//! Each test runs the binary with `$HOME`/XDG pinned to a scratch directory
//! and an empty scratch cwd, so config discovery sees only what the test
//! wrote (same env pinning as `replay_mode.rs`'s `ScratchStation`).

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const README_AUTONOMOUS_TOGGLE: &str = "[autonomous]\nenabled = true\n";

const GUIDE_SAME_HOST_UDP: &str = r#"[network.wsjtx_udp]
enabled = true
destination = "127.0.0.1:2237"
"#;

const GUIDE_CROSS_MACHINE_UDP: &str = r#"[network.wsjtx_udp]
enabled = true
destination = "224.0.0.73:2237"     # multicast group; GridTracker's recommended choice
multicast_interface = "192.168.1.50" # A's real LAN NIC IP — NOT "" (loopback-only default)
multicast_ttl = 3
"#;

const SYNTAX_ERROR: &str = "[station\n";

/// Malformed on line 2 (comment first), so a diagnostic has to name the
/// parser's real source line instead of a hardcoded `line 1`.
const SYNTAX_ERROR_LINE_2: &str = "# PAN-301\n[station\n";

/// Parses, then fails `Config::validate()`: the brick-by-values counterpart
/// to a parse error (same fixture the show/doctor tests use).
const VALIDATION_ERROR: &str = "[autonomous]\ncq_after_idle_cycles = 0\n";

/// A scratch `$HOME` and an empty scratch working directory.
struct Scratch {
    home: TempDir,
    cwd: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    /// `~/.pancetta/pancetta.toml` inside the scratch home.
    fn default_config_path(&self) -> PathBuf {
        self.home.path().join(".pancetta").join("pancetta.toml")
    }

    /// Write `text` to `~/.pancetta/pancetta.toml` (auto-discovered).
    fn write_default_config(&self, text: &str) -> PathBuf {
        let path = self.default_config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Write `text` to a named file outside every search path (for `--config`).
    fn write_file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The real binary, pinned to this scratch home and cwd. `--headless` so
    /// no wizard can ever run.
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.env("HOME", self.home.path())
            // `dirs` consults these before `$HOME` on Linux/macOS.
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            // `dirs::home_dir()` uses the profile directory on Windows.
            .env("USERPROFILE", self.home.path())
            .env_remove("PANCETTA_CALLSIGN")
            // Startup-only children: no macOS responsibility relaunch
            // (PAN-264), and a pinned filter so an inherited RUST_LOG cannot
            // hide this crate's info-level startup boundary log.
            .env("PANCETTA_MACOS_RELAUNCH", "0")
            .env("RUST_LOG", "pancetta=info,warn")
            .current_dir(self.cwd.path())
            .timeout(Duration::from_secs(60))
            .arg("--headless");
        cmd
    }

    /// The same isolated binary with no run-mode flag, as a plain std
    /// command so a test can attach a pseudo-terminal or null stdin itself
    /// (PAN-301 interactive and unattended startup). Keep the env list in
    /// sync with [`Scratch::command`].
    fn raw_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_pancetta"));
        cmd.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("USERPROFILE", self.home.path())
            .env("PANCETTA_MACOS_RELAUNCH", "0")
            .env("RUST_LOG", "pancetta=info,warn")
            .env_remove("PANCETTA_CALLSIGN")
            .current_dir(self.cwd.path());
        cmd
    }

    fn validate(&self, config: Option<&Path>) -> std::process::Output {
        let mut cmd = self.command();
        if let Some(p) = config {
            cmd.arg("--config").arg(p);
        }
        cmd.args(["config", "--validate"]).output().unwrap()
    }
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Gherkin scenario 3: a malformed config cannot pass validation silently.
/// PAN-301: exit exactly 1, name the file and the parser's source line, and
/// never offer repair or the wizard.
#[test]
fn validate_fails_on_auto_discovered_syntax_error() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR_LINE_2);
    let out = s.validate(None);
    let so = stdout(&out);
    let se = stderr(&out);
    let all = format!("{so}{se}");
    assert_eq!(out.status.code(), Some(1), "stdout: {so}\nstderr: {se}");
    assert!(
        so.contains(&format!("Config file: {} — FAILED TO LOAD", path.display())),
        "{all}"
    );
    assert!(so.contains("Configuration validation: FAIL"), "{all}");
    assert!(!so.contains("PASS"), "{all}");
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert!(!all.contains("First-Run Setup"), "{all}");
}

/// PAN-301: the explicit `--config` counterpart reports the same parse
/// diagnostic and exits exactly 1.
#[test]
fn validate_fails_on_explicit_syntax_error() {
    let s = Scratch::new();
    let path = s.write_file("broken-explicit.toml", SYNTAX_ERROR_LINE_2);
    let out = s.validate(Some(&path));
    let so = stdout(&out);
    let se = stderr(&out);
    let all = format!("{so}{se}");
    assert_eq!(out.status.code(), Some(1), "stdout: {so}\nstderr: {se}");
    assert!(
        so.contains(&format!("Config file: {} — FAILED TO LOAD", path.display())),
        "{all}"
    );
    assert!(so.contains("Configuration validation: FAIL"), "{all}");
    assert!(!so.contains("PASS"), "{all}");
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert!(!all.contains("First-Run Setup"), "{all}");
}

#[test]
fn validate_fails_on_auto_discovered_type_error() {
    let s = Scratch::new();
    s.write_default_config("[autonomous]\nenabled = \"yes\"\n");
    let out = s.validate(None);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    assert!(stdout(&out).contains("Configuration validation: FAIL"));
}

/// A file that parses but fails `Config::validate()` must fail `--show` too,
/// as it fails `--validate` and startup.
#[test]
fn show_fails_on_auto_discovered_invalid_config() {
    let s = Scratch::new();
    s.write_default_config("[autonomous]\ncq_after_idle_cycles = 0\n");
    let show = s.command().args(["config", "--show"]).output().unwrap();
    assert!(!show.status.success(), "stdout: {}", stdout(&show));
    assert!(
        stderr(&show).contains("cq_after_idle_cycles"),
        "stderr: {}",
        stderr(&show)
    );
    assert!(!s.validate(None).status.success());
}

#[test]
fn validate_and_doctor_agree_on_broken_file() {
    for broken in [SYNTAX_ERROR, "[autonomous]\ncq_after_idle_cycles = 0\n"] {
        let s = Scratch::new();
        s.write_default_config(broken);
        let validate = s.validate(None);
        let doctor = s.command().arg("doctor").output().unwrap();
        assert!(
            !validate.status.success(),
            "validate: {}",
            stdout(&validate)
        );
        assert_eq!(doctor.status.code(), Some(3), "doctor: {}", stdout(&doctor));
        assert!(stdout(&doctor).contains("Result: NOT READY."));
    }
}

/// Gherkin scenario 1, end to end: the README toggle, auto-discovered.
#[test]
fn validate_passes_readme_recipe_auto_discovered() {
    let s = Scratch::new();
    let path = s.write_default_config(README_AUTONOMOUS_TOGGLE);
    let out = s.validate(None);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        text.contains(&format!("Config file: {}", path.display())),
        "{text}"
    );
    assert!(text.contains("Configuration validation: PASS"), "{text}");
}

/// Gherkin scenario 2, end to end: both GUIDE.md GridTracker recipes.
#[test]
fn validate_passes_guide_udp_recipes_via_config_flag() {
    let s = Scratch::new();
    for (name, recipe) in [
        ("same_host.toml", GUIDE_SAME_HOST_UDP),
        ("cross_machine.toml", GUIDE_CROSS_MACHINE_UDP),
    ] {
        let path = s.write_file(name, recipe);
        let out = s.validate(Some(&path));
        assert!(
            out.status.success(),
            "{name}: stdout: {}\nstderr: {}",
            stdout(&out),
            stderr(&out)
        );
        assert!(stdout(&out).contains("Configuration validation: PASS"));
    }
}

#[test]
fn validate_with_no_config_file_reports_defaults() {
    let s = Scratch::new();
    let out = s.validate(None);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("No config file found"), "{text}");
    assert!(text.contains("N0CALL"), "{text}");
}

#[test]
fn validate_fails_on_broken_file_in_cwd() {
    let s = Scratch::new();
    let path = s.cwd.path().join("pancetta.toml");
    std::fs::write(&path, SYNTAX_ERROR).unwrap();
    let out = s.validate(None);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    // The error names the file it could not load: the cwd copy. The binary
    // sees its cwd with symlinks resolved, so compare against that form.
    let cwd_file = std::fs::canonicalize(s.cwd.path())
        .unwrap()
        .join("pancetta.toml");
    assert!(
        stderr(&out).contains(&format!(
            "config file {} failed to load",
            cwd_file.display()
        )),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains(&format!(
            "{} — FAILED TO LOAD",
            path.file_name().unwrap().to_string_lossy()
        )),
        "stdout: {}",
        stdout(&out)
    );
}

#[test]
fn validate_reports_unknown_key_warning_for_config_flag() {
    let s = Scratch::new();
    let path = s.write_file("typo.toml", "[network.wsjtx_udp]\nenable = true\n");
    let out = s.validate(Some(&path));
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("Unknown config key `enable`"), "{all}");
}

/// PAN-301 scenario 1: plain `--headless` startup (no WAV fixture) stops on
/// a malformed discovered file before station initialization — nonzero exit
/// naming the file and the parser's source line, no success log (info
/// logging is on), no repair offer, file untouched. Both discovery
/// locations, the home file and the cwd file, are covered separately.
#[test]
fn headless_startup_refuses_broken_config() {
    // Discovered ~/.pancetta/pancetta.toml.
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR_LINE_2);
    let before = std::fs::read(&path).unwrap();
    let out = s.command().output().unwrap();
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(
        stderr(&out).contains(&path.display().to_string()),
        "stderr: {}",
        stderr(&out)
    );
    assert!(all.contains("failed to load"), "{all}");
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Configuration loaded successfully"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "file changed");

    // Discovered pancetta.toml in the working directory. The binary sees
    // its cwd with symlinks resolved, so compare against that form.
    let s = Scratch::new();
    let path = s.cwd.path().join("pancetta.toml");
    std::fs::write(&path, SYNTAX_ERROR_LINE_2).unwrap();
    let before = std::fs::read(&path).unwrap();
    let out = s.command().output().unwrap();
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(1), "{all}");
    let cwd_file = std::fs::canonicalize(s.cwd.path())
        .unwrap()
        .join("pancetta.toml");
    assert!(
        stderr(&out).contains(&cwd_file.display().to_string()),
        "stderr: {}",
        stderr(&out)
    );
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Configuration loaded successfully"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "file changed");
}

/// PAN-301: unattended startup WITHOUT `--headless` and without a terminal
/// on stdin fails on the malformed discovered file instead of prompting —
/// `is_terminal()` is false, so the repair offer must stay off.
#[test]
fn nonterminal_syntax_error_never_offers_repair() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR_LINE_2);
    let before = std::fs::read(&path).unwrap();
    let out = s
        .raw_command()
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(
        all.contains(&path.display().to_string()) && all.contains("failed to load"),
        "{all}"
    );
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert!(!all.contains("First-Run Setup"), "{all}");
    assert!(!all.contains("Edit the file by hand"), "{all}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "file changed");
}

/// `config --generate` writes the same header-annotated, drift-tested text
/// as `pancetta-config/defaults.toml` -- no random `[metadata]` block.
#[test]
fn generate_writes_the_defaults_toml_text() {
    let s = Scratch::new();
    let out_path = s.home.path().join("g.toml");
    let out = s
        .command()
        .args(["config", "--generate"])
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let generated = std::fs::read_to_string(&out_path).unwrap();
    let checked_in = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pancetta-config/defaults.toml"
    ))
    .unwrap();
    assert_eq!(generated, checked_in);
    assert!(!generated.contains("[metadata]"));
}

/// `config --generate` into a directory that does not exist yet creates it
/// and writes owner-only, as `save_to_file` does: operators add credentials
/// to the generated file.
#[test]
fn generate_creates_missing_parent_directories_owner_only() {
    let s = Scratch::new();
    let out_path = s.home.path().join("new").join("dir").join("pancetta.toml");
    let out = s
        .command()
        .args(["config", "--generate"])
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(out_path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&out_path), 0o600);
        assert_eq!(mode(out_path.parent().unwrap()), 0o700);
    }
}

#[test]
fn doctor_lists_field_checks() {
    let scratch = Scratch::new();
    let out = scratch.command().arg("doctor").output().unwrap();
    let text = stdout(&out);
    assert!(text.contains("10 checks"), "{text}");
    let labels = [
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
    let rows: Vec<_> = text.lines().filter(|line| line.starts_with('[')).collect();
    assert_eq!(rows.len(), labels.len(), "{text}");
    for (row, label) in rows.iter().zip(labels) {
        assert!(row.contains(label), "{row}");
    }
    assert!(text.contains("not applicable on this OS") || cfg!(target_os = "macos"));
    for line in text.lines().filter(|line| line.contains("fix:")) {
        assert!(line.starts_with("       fix: "));
    }
}

#[test]
fn doctor_missing_config_exits_2() {
    let s = Scratch::new();
    for explicit in [false, true] {
        let mut cmd = s.command();
        if explicit {
            cmd.arg("--config").arg(s.home.path().join("missing.toml"));
        }
        let out = cmd.arg("doctor").output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", stdout(&out));
        assert!(stdout(&out).contains(
            "Result: NOT CONFIGURED. Run `pancetta setup`, then re-run `pancetta doctor`."
        ));
    }
}

fn assert_doctor_json(out: &std::process::Output, verdict: &str, code: i32) {
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: stdout={} stderr={}", stdout(out), stderr(out)));
    assert_eq!(out.status.code(), Some(code));
    assert_eq!(report["verdict"], verdict);
    assert_eq!(report["exit_code"], code);
    assert!(report["config_path"].is_string());
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 10);
    for check in checks {
        assert!(check["name"].is_string());
        assert!(["PASS", "WARN", "FAIL"].contains(&check["status"].as_str().unwrap()));
        assert!(check["detail"].is_string());
        assert!(check.as_object().unwrap().contains_key("fix"));
        assert!(check["fix"].is_null() || check["fix"].is_string());
    }
    assert!(out.stdout.ends_with(b"\n"));
}

#[test]
fn doctor_json_is_one_document() {
    let s = Scratch::new();
    let missing = s.command().args(["doctor", "--json"]).output().unwrap();
    assert_doctor_json(&missing, "NOT CONFIGURED", 2);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = s
            .home
            .path()
            .join(std::ffi::OsString::from_vec(b"missing-\xff.toml".to_vec()));
        let out = s
            .command()
            .arg("--config")
            .arg(&path)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        assert_doctor_json(&out, "NOT CONFIGURED", 2);
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["config_path"], path.to_string_lossy().as_ref());
    }
    s.write_default_config(SYNTAX_ERROR);
    let invalid = s.command().args(["doctor", "--json"]).output().unwrap();
    assert_doctor_json(&invalid, "NOT READY", 3);
}

#[test]
fn doctor_json_logging_stays_on_stderr() {
    let s = Scratch::new();
    for format in ["text", "json"] {
        for verbose in [false, true] {
            let mut cmd = s.command();
            cmd.args(["--log-format", format]);
            if verbose {
                cmd.arg("--verbose");
            }
            let out = cmd.args(["doctor", "--json"]).output().unwrap();
            assert_doctor_json(&out, "NOT CONFIGURED", 2);
            assert!(stderr(&out).contains("Starting Pancetta"));
        }
    }
}

#[test]
fn station_agent_public_plaintext_fails_validation() {
    for (field, value, scheme) in [
        ("relay_url", "ws://service.example/agent", "wss"),
        ("pairing_api_url", "http://cqdx.io/api/v1", "https"),
    ] {
        let scratch = Scratch::new();
        let path = scratch.write_file(
            "plaintext.toml",
            &format!("[network.station_agent]\n{field} = {value:?}\n"),
        );
        let output = scratch.validate(Some(&path));
        assert!(!output.status.success());
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains(&format!(
                "network.station_agent.{field} must use {scheme}://"
            )),
            "{text}"
        );
        assert!(stdout(&output).contains("Configuration validation: FAIL"));
    }
}

#[test]
fn pair_override_rejects_plaintext_before_identity() {
    let scratch = Scratch::new();
    let key_dir = scratch.home.path().join("station-identity");
    let path = scratch.write_file(
        "pair.toml",
        &format!(
            "[network.station_agent]\npairing_api_url = \"https://cqdx.io/api/v1\"\nkey_dir = {:?}\n",
            key_dir.to_string_lossy()
        ),
    );
    let output = scratch
        .command()
        .arg("--config")
        .arg(&path)
        .args([
            "pair",
            "PAN310-SECRET-CODE",
            "--pairing-api-url",
            "http://cqdx.io/api/v1",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("network.station_agent.pairing_api_url must use https://"),
        "{text}"
    );
    assert!(!text.contains("Loading (or generating)"));
    assert!(!text.contains("Enrolling"));
    assert!(!text.contains("PAN310-SECRET-CODE"));
    assert!(
        !key_dir.exists(),
        "identity directory was created before validation"
    );
    assert!(!scratch.home.path().join(".pancetta/agent").exists());
}

/// PAN-301: a station child with its stdin on a pseudo-terminal slave, so
/// `is_terminal()` is true and prompts render, while stdout and stderr stay
/// piped and are drained concurrently into separate transcripts.
#[cfg(unix)]
mod terminal {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// How long a child may take to show expected output, or to exit.
    const DEADLINE: Duration = Duration::from_secs(30);

    pub(super) struct TerminalChild {
        child: Child,
        master: File,
        stdout: Arc<Mutex<String>>,
        stderr: Arc<Mutex<String>>,
        readers: Vec<std::thread::JoinHandle<()>>,
    }

    /// Read `pipe` to EOF into a shared transcript on its own thread, so a
    /// chatty child can never fill the pipe and block.
    fn drain(mut pipe: Box<dyn Read + Send>) -> (Arc<Mutex<String>>, std::thread::JoinHandle<()>) {
        let text = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&text);
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
        });
        (text, handle)
    }

    impl TerminalChild {
        pub(super) fn spawn(mut cmd: Command) -> Self {
            // SAFETY: openpty receives two writable fd slots and a valid
            // winsize; the null name/termios pointers request defaults. The
            // return value is checked before any fd is used, and each fd is
            // moved into exactly one owned File — the slave is consumed by
            // the child's stdin config, so the parent's copy closes at spawn
            // and the master stays open for input.
            let (master, slave) = unsafe {
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
                        std::ptr::null_mut(),
                        &size
                    ),
                    0
                );
                (File::from_raw_fd(master), File::from_raw_fd(slave))
            };
            cmd.env("TERM", "xterm-256color")
                .stdin(slave)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("failed to spawn pancetta");
            let (stdout, out_reader) = drain(Box::new(child.stdout.take().expect("stdout piped")));
            let (stderr, err_reader) = drain(Box::new(child.stderr.take().expect("stderr piped")));
            Self {
                child,
                master,
                stdout,
                stderr,
                readers: vec![out_reader, err_reader],
            }
        }

        /// Everything the child has printed so far, both streams.
        pub(super) fn combined(&self) -> String {
            format!(
                "{}{}",
                self.stdout.lock().unwrap(),
                self.stderr.lock().unwrap()
            )
        }

        pub(super) fn stdout_text(&self) -> String {
            self.stdout.lock().unwrap().clone()
        }

        /// Block until `needle` appears in the combined transcript; a child
        /// that never prints it fails the test at the deadline with the
        /// transcript instead of hanging.
        pub(super) fn wait_for(&self, needle: &str) {
            let deadline = Instant::now() + DEADLINE;
            while !self.combined().contains(needle) {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {needle:?}; transcript:\n{}",
                    self.combined()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Send one line of terminal input through the pty master.
        pub(super) fn reply(&mut self, line: &[u8]) {
            self.master.write_all(line).expect("write to pty master");
        }

        /// Wait for the child to exit on its own within the deadline, then
        /// join the readers so both transcripts are complete.
        pub(super) fn wait_exit(&mut self) -> std::process::ExitStatus {
            let deadline = Instant::now() + DEADLINE;
            loop {
                if let Some(status) = self.child.try_wait().expect("try_wait child") {
                    self.join_readers();
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "child did not exit; transcript:\n{}",
                    self.combined()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn join_readers(&mut self) {
            for reader in self.readers.drain(..) {
                let _ = reader.join();
            }
        }
    }

    impl Drop for TerminalChild {
        /// Cleanup guard: a live child (a failed assertion, or a test that
        /// stops at a prompt by design) is killed and reaped so nothing
        /// leaks past the test.
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.join_readers();
        }
    }
}

/// PAN-301 scenario 2: interactive startup on a broken discovered file
/// offers the de-brick wizard for a syntax error AND a validation error.
/// Declining — explicit `n`, or Enter taking the default No — exits nonzero,
/// leaves the file byte-identical, and creates no backup.
#[cfg(unix)]
#[test]
fn interactive_broken_config_offers_repair_and_decline_preserves_file() {
    for (label, broken, diagnostic) in [
        ("syntax", SYNTAX_ERROR_LINE_2, "TOML parse error at line 2"),
        ("validation", VALIDATION_ERROR, "cq_after_idle_cycles"),
    ] {
        for reply in ["n\n", "\n"] {
            let s = Scratch::new();
            let path = s.write_default_config(broken);
            let before = std::fs::read(&path).unwrap();
            let mut child = terminal::TerminalChild::spawn(s.raw_command());
            child.wait_for(
                "Re-run first-time setup? (overwrites the broken config on save) [y/N]: ",
            );
            let offered = child.combined();
            assert!(
                offered.contains("ERROR: your saved configuration failed to load:"),
                "{label} {reply:?}: {offered}"
            );
            assert!(
                offered.contains(&path.display().to_string()),
                "{label} {reply:?}: {offered}"
            );
            assert!(offered.contains(diagnostic), "{label} {reply:?}: {offered}");
            child.reply(reply.as_bytes());
            let status = child.wait_exit();
            let after = child.combined();
            assert_eq!(status.code(), Some(1), "{label} {reply:?}: {after}");
            assert!(
                after.contains("Edit the file by hand, or delete it to start fresh."),
                "{label} {reply:?}: {after}"
            );
            assert!(
                !after.contains("=== Pancetta First-Run Setup ==="),
                "{label} {reply:?}: {after}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "{label} {reply:?}: file changed"
            );
            let bak = path.with_file_name(format!(
                "{}.bak",
                path.file_name().unwrap().to_string_lossy()
            ));
            assert!(
                !bak.exists(),
                "{label} {reply:?}: backup {} was created",
                bak.display()
            );
        }
    }
}

/// PAN-301 scenario 2 (accept): Yes enters the existing first-run setup
/// wizard before any station initialization. The test stops at the callsign
/// prompt — the N0CALL shown there is the wizard's own default, not a
/// station run — killing and reaping the child at that point, with the
/// broken file still unchanged.
#[cfg(unix)]
#[test]
fn interactive_syntax_error_accept_enters_setup() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR_LINE_2);
    let before = std::fs::read(&path).unwrap();
    let mut child = terminal::TerminalChild::spawn(s.raw_command());
    child.wait_for("Re-run first-time setup? (overwrites the broken config on save) [y/N]: ");
    child.reply(b"y\n");
    child.wait_for("=== Pancetta First-Run Setup ===");
    child.wait_for("Callsign [N0CALL]:");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "file changed before any wizard save"
    );
    // Wizard entry is the claim; nothing further is answered. Drop kills
    // and reaps the child.
}

/// PAN-301: `config --validate` never offers repair even with a real
/// terminal on stdin and no `--headless` — it reports and exits 1 without
/// reading any input.
#[cfg(unix)]
#[test]
fn terminal_validate_syntax_error_never_offers_repair() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR_LINE_2);
    let mut cmd = s.raw_command();
    cmd.args(["config", "--validate"]);
    let mut child = terminal::TerminalChild::spawn(cmd);
    // No input is sent: a prompt here would block until the deadline and
    // fail the wait below with the transcript.
    let status = child.wait_exit();
    let so = child.stdout_text();
    let all = child.combined();
    assert_eq!(status.code(), Some(1), "{all}");
    assert!(
        so.contains(&format!("Config file: {} — FAILED TO LOAD", path.display())),
        "{all}"
    );
    assert!(so.contains("Configuration validation: FAIL"), "{all}");
    assert!(!so.contains("PASS"), "{all}");
    assert!(all.contains("TOML parse error at line 2"), "{all}");
    assert!(!all.contains("Re-run first-time setup"), "{all}");
    assert!(!all.contains("First-Run Setup"), "{all}");
}
