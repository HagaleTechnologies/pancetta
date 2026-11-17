//! PAN-385: `[[autonomous.band_plan]]` and the new `[autonomous.band_hopping]`
//! keys — parsing, defaults, validation and the hopping-disabled warning.
use super::*;
use crate::Config;

/// The ticket's two-window plan from the plan's "Design for review".
const TICKET_EXAMPLE: &str = r#"
[autonomous.band_hopping]
enabled = true
hop_threshold = 20
needed_activity = true
needed_idle_secs = 600
grey_line = true

[[autonomous.band_hopping.bands]]
band_name = "20m"
dial_frequency = 14074000
priority = 1

[[autonomous.band_hopping.bands]]
band_name = "17m"
dial_frequency = 18100000
priority = 2

[[autonomous.band_plan]]
utc = "00:00-06:00"
bands = ["40m", "80m"]

[[autonomous.band_plan]]
utc = "06:00-24:00"
bands = ["20m", "17m"]
"#;

const TWO_WINDOWS_WARNING: &str = "autonomous.band_plan has 2 windows but autonomous.band_hopping.enabled is false; the plan is not used";

fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pancetta.toml");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn window(utc: &str, bands: &[&str]) -> BandPlanWindow {
    BandPlanWindow {
        utc: utc.to_string(),
        bands: bands.iter().map(|b| b.to_string()).collect(),
    }
}

fn plan_warnings(warnings: &[String]) -> Vec<&String> {
    warnings
        .iter()
        .filter(|w| w.contains("autonomous.band_plan"))
        .collect()
}

fn invalid_value(config: &Config) -> (String, String) {
    match config.validate() {
        Err(ConfigError::InvalidValue { field, value }) => (field, value),
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn band_plan_parses_the_ticket_example_without_warnings() {
    let (_dir, path) = write_config(TICKET_EXAMPLE);
    let (config, warnings) = Config::load_from_file_with_warnings(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        config.autonomous.band_plan,
        vec![
            window("00:00-06:00", &["40m", "80m"]),
            window("06:00-24:00", &["20m", "17m"]),
        ]
    );
    assert!(config.autonomous.band_hopping.enabled);
    assert_eq!(config.autonomous.band_hopping.bands.len(), 2);
    assert_eq!(config.autonomous.band_plan_warning(), None);
}

#[test]
fn band_hopping_new_keys_default_when_absent() {
    let config = Config::default();
    assert!(config.autonomous.band_plan.is_empty());
    assert!(config.autonomous.band_hopping.needed_activity);
    assert_eq!(config.autonomous.band_hopping.needed_idle_secs, 600);
    assert!(config.autonomous.band_hopping.grey_line);

    let parsed: AutonomousConfig = toml::from_str(
        "[band_hopping]\nenabled = true\nhop_threshold = 15\n\n\
         [[band_hopping.bands]]\nband_name = \"20m\"\ndial_frequency = 14074000\npriority = 1\n",
    )
    .unwrap();
    assert!(parsed.band_hopping.enabled);
    assert_eq!(parsed.band_hopping.hop_threshold, 15);
    assert!(parsed.band_hopping.needed_activity);
    assert_eq!(parsed.band_hopping.needed_idle_secs, 600);
    assert!(parsed.band_hopping.grey_line);
    assert!(parsed.band_plan.is_empty());
}

#[test]
fn band_plan_validation_rejects_each_bad_value() {
    let mut config = Config::default();
    config.autonomous.band_hopping.hop_threshold = 0;
    assert_eq!(
        invalid_value(&config),
        (
            "autonomous.band_hopping.hop_threshold".to_string(),
            "0".to_string()
        )
    );

    let mut config = Config::default();
    config.autonomous.band_hopping.bands[1].dial_frequency = 1;
    assert_eq!(
        invalid_value(&config),
        (
            "autonomous.band_hopping.bands[1].dial_frequency".to_string(),
            "1".to_string()
        )
    );

    for bad in ["25:00-06:00", "00:00-00:00"] {
        let mut config = Config::default();
        config.autonomous.band_plan = vec![window(bad, &["40m"])];
        assert_eq!(
            invalid_value(&config),
            ("autonomous.band_plan[0].utc".to_string(), bad.to_string())
        );
    }

    let mut config = Config::default();
    config.autonomous.band_plan = vec![window("00:00-06:00", &[])];
    assert_eq!(
        invalid_value(&config),
        (
            "autonomous.band_plan[0].bands".to_string(),
            "[]".to_string()
        )
    );

    for bad in ["11m", "70cm"] {
        let mut config = Config::default();
        config.autonomous.band_plan = vec![
            window("00:00-06:00", &["40m"]),
            window("06:00-24:00", &["20m", bad]),
        ];
        assert_eq!(
            invalid_value(&config),
            ("autonomous.band_plan[1].bands".to_string(), bad.to_string())
        );
    }
}

#[test]
fn band_plan_validation_accepts_wrap_and_full_day() {
    for utc in ["22:00-02:00", "18:00-24:00", "00:00-24:00"] {
        let mut config = Config::default();
        config.autonomous.band_plan = vec![window(utc, &["40m", "80m"])];
        config.validate().unwrap_or_else(|e| panic!("{utc}: {e}"));
    }

    let (_dir, path) = write_config(TICKET_EXAMPLE);
    let (config, _) = Config::load_from_file_with_warnings(&path).unwrap();
    config.validate().unwrap();
}

/// Serialises the HOME/XDG override below against itself; the default
/// loader reads both through `dirs`.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores HOME and XDG_CONFIG_HOME on drop, panic included.
struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn load_default_warnings_with_home(text: &str) -> Vec<String> {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join(".pancetta");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("pancetta.toml"), text).unwrap();

    let _restore = RestoreEnv(
        ["HOME", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
    );
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
    let (_config, warnings) = Config::load_default_with_warnings().unwrap();
    warnings
}

#[test]
fn band_plan_without_band_hopping_warns() {
    let disabled = TICKET_EXAMPLE.replace("enabled = true", "enabled = false");
    let (_dir, path) = write_config(&disabled);

    let (config, warnings) = Config::load_from_file_with_warnings(&path).unwrap();
    assert_eq!(warnings, vec![TWO_WINDOWS_WARNING.to_string()]);
    assert_eq!(config.autonomous.band_plan.len(), 2);

    let (_, report) = Config::load_from_file_with_report(&path).unwrap();
    assert_eq!(report.warnings, vec![TWO_WINDOWS_WARNING.to_string()]);

    let warnings = load_default_warnings_with_home(&disabled);
    assert_eq!(plan_warnings(&warnings), vec![TWO_WINDOWS_WARNING]);

    // Hopping enabled, or an empty plan: no warning on any path.
    let (_dir, enabled_path) = write_config(TICKET_EXAMPLE);
    let (_, warnings) = Config::load_from_file_with_warnings(&enabled_path).unwrap();
    assert!(plan_warnings(&warnings).is_empty(), "{warnings:?}");
    let (_, report) = Config::load_from_file_with_report(&enabled_path).unwrap();
    assert!(plan_warnings(&report.warnings).is_empty(), "{report:?}");
    let warnings = load_default_warnings_with_home(TICKET_EXAMPLE);
    assert!(plan_warnings(&warnings).is_empty(), "{warnings:?}");

    let (_dir, empty_path) = write_config("[autonomous.band_hopping]\nenabled = false\n");
    let (_, warnings) = Config::load_from_file_with_warnings(&empty_path).unwrap();
    assert!(plan_warnings(&warnings).is_empty(), "{warnings:?}");

    // One window reads singular.
    let one = AutonomousConfig {
        band_plan: vec![window("00:00-06:00", &["40m"])],
        ..AutonomousConfig::default()
    };
    assert_eq!(
        one.band_plan_warning().as_deref(),
        Some("autonomous.band_plan has 1 window but autonomous.band_hopping.enabled is false; the plan is not used")
    );
}
