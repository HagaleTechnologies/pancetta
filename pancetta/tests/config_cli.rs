//! PAN-90: `pancetta config --validate` and startup against the real binary.
//!
//! Every documented minimal recipe must pass validation on its own, and a
//! config file that exists but fails to load must fail validation (and
//! startup) with a non-zero exit naming the file -- never silently fall back
//! to an all-defaults (N0CALL) config while printing PASS.
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
            .current_dir(self.cwd.path())
            .timeout(Duration::from_secs(60))
            .arg("--headless");
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
#[test]
fn validate_fails_on_auto_discovered_syntax_error() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR);
    let out = s.validate(None);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    assert!(
        stdout(&out).contains("Configuration validation: FAIL"),
        "stdout: {}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("PASS"), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains(&path.display().to_string()),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn validate_fails_on_auto_discovered_type_error() {
    let s = Scratch::new();
    s.write_default_config("[autonomous]\nenabled = \"yes\"\n");
    let out = s.validate(None);
    assert!(!out.status.success(), "stdout: {}", stdout(&out));
    assert!(stdout(&out).contains("Configuration validation: FAIL"));
}

#[test]
fn validate_and_doctor_agree_on_broken_file() {
    let s = Scratch::new();
    s.write_default_config(SYNTAX_ERROR);
    let validate = s.validate(None);
    let doctor = s.command().arg("doctor").output().unwrap();
    assert!(
        !validate.status.success(),
        "validate: {}",
        stdout(&validate)
    );
    assert!(!doctor.status.success(), "doctor: {}", stdout(&doctor));
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

#[test]
fn headless_startup_refuses_broken_config() {
    let s = Scratch::new();
    let path = s.write_default_config(SYNTAX_ERROR);
    let missing_wav = s.home.path().join("missing.wav");
    let out = s.command().arg("--wav").arg(&missing_wav).output().unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains(&path.display().to_string()), "stderr: {err}");
    assert!(err.contains("failed to load"), "stderr: {err}");
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
