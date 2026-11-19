use crate::{Config, ConfigSection};

#[test]
fn auto_drive_defaults_round_trip_and_merge() {
    let defaults = Config::default();
    assert!(defaults.audio.auto_drive.enabled);
    assert_eq!(defaults.audio.auto_drive.alc_onset, 0.10);
    assert_eq!(defaults.audio.auto_drive.step_up_db, 0.5);
    assert_eq!(defaults.audio.auto_drive.step_down_db, 2.0);
    assert_eq!(defaults.audio.auto_drive.start_db, -6.0);
    assert_eq!(defaults.audio.levels.output_gain_floor_db, -20.0);
    let toml = toml::to_string(&defaults.audio).unwrap();
    let json = serde_json::to_string(&defaults.audio).unwrap();
    assert!(
        toml::from_str::<crate::audio::AudioConfig>(&toml)
            .unwrap()
            .auto_drive
            .enabled
    );
    assert!(
        serde_json::from_str::<crate::audio::AudioConfig>(&json)
            .unwrap()
            .auto_drive
            .enabled
    );
    let mut merged = crate::audio::AudioConfig::default();
    let overlay = toml::from_str::<crate::audio::AudioConfig>(
        "[auto_drive]\nenabled = false\nstep_up_db = 0.75\n",
    )
    .unwrap();
    merged.merge_with(overlay);
    assert!(!merged.auto_drive.enabled);
    assert_eq!(merged.auto_drive.step_up_db, 0.75);
}

#[test]
fn auto_drive_validates_limits_and_clamps_legacy_gain() {
    let mut config = Config::default();
    config.audio.levels.output_gain_floor_db = -10.0;
    config.audio.levels.output_gain_db = -18.0;
    config.clamp_tx_drive();
    assert_eq!(config.audio.levels.output_gain_db, -10.0);
    config.audio.auto_drive.alc_onset = 0.5;
    assert!(config
        .validate()
        .unwrap_err()
        .to_string()
        .contains("audio.auto_drive.alc_onset"));
    config.audio.auto_drive.enabled = false;
    assert!(config.validate().is_ok());
    config.audio.levels.output_gain_floor_db = -21.0;
    assert!(config
        .validate()
        .unwrap_err()
        .to_string()
        .contains("output_gain_floor_db"));
}

#[test]
fn auto_drive_rejects_nonfinite_and_nonpositive_controls() {
    for (field, value) in [
        ("alc_onset", f32::NAN),
        ("step_up_db", 0.0),
        ("step_down_db", -1.0),
        ("start_db", f32::INFINITY),
    ] {
        let mut config = Config::default();
        match field {
            "alc_onset" => config.audio.auto_drive.alc_onset = value,
            "step_up_db" => config.audio.auto_drive.step_up_db = value,
            "step_down_db" => config.audio.auto_drive.step_down_db = value,
            "start_db" => config.audio.auto_drive.start_db = value,
            _ => unreachable!(),
        }
        assert!(config
            .audio
            .validate_section()
            .unwrap_err()
            .to_string()
            .contains(field));
    }
}

#[test]
fn auto_drive_onset_uses_effective_rig_limit() {
    let mut config = Config::default();
    config.rig.power_control.protection.max_alc = 0.2;
    config.audio.auto_drive.alc_onset = 0.2;
    assert!(config
        .validate()
        .unwrap_err()
        .to_string()
        .contains("alc_onset"));
    config.audio.auto_drive.alc_onset = 0.19;
    assert!(config.validate().is_ok());
    config.rig.power_control.protection.alc_overdrive_warning = false;
    config.rig.power_control.protection.max_alc = 2.0;
    config.audio.auto_drive.alc_onset = 0.5;
    assert!(config
        .validate()
        .unwrap_err()
        .to_string()
        .contains("alc_onset"));
}

#[test]
fn drive_validates_and_bounds_legacy_gain() {
    let mut config = Config::default();
    assert_eq!(config.audio.levels.output_gain_db, 0.0);
    assert_eq!(config.audio.levels.output_gain_ceiling_db, 0.0);
    config.audio.levels.output_gain_db = -24.0;
    assert!(config.clamp_tx_drive().unwrap().contains("-24"));
    assert_eq!(config.audio.levels.output_gain_db, -20.0);
    config.audio.levels.output_gain_db = 4.25;
    config.audio.levels.output_gain_ceiling_db = 5.0;
    assert!(config.clamp_tx_drive().is_none());
    assert_eq!(config.audio.levels.output_gain_db, 4.25);
    config.audio.levels.output_gain_ceiling_db = 6.0;
    assert!(config
        .audio
        .validate_section()
        .unwrap_err()
        .to_string()
        .contains("output_gain_ceiling_db"));
    config.audio.levels.output_gain_ceiling_db = 0.0;
    config.audio.levels.output_gain_db = f32::NAN;
    assert!(config
        .audio
        .validate_section()
        .unwrap_err()
        .to_string()
        .contains("output_gain_db"));
}

#[test]
fn save_drive_preserves_toml_comments_and_json_neighbors() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("station.toml");
    std::fs::write(
        &toml_path,
        "# operator note\n[audio.levels]\noutput_gain_db = 0.0 # drive\n[rig]\nmodel = \"old\"\n",
    )
    .unwrap();
    Config::save_output_gain_db(&toml_path, true, -4.0).unwrap();
    let saved = std::fs::read_to_string(&toml_path).unwrap();
    assert!(saved.contains("# operator note"));
    assert!(saved.contains("# drive"));
    assert!(saved.contains("model = \"old\""));
    assert_eq!(
        Config::load_from_file(&toml_path)
            .unwrap()
            .audio
            .levels
            .output_gain_db,
        -4.0
    );

    let json_path = dir.path().join("station.json");
    std::fs::write(
        &json_path,
        r#"{"audio":{"levels":{"output_gain_db":0},"output_device":"USB"},"other":{"keep":true}}"#,
    )
    .unwrap();
    Config::save_output_gain_db(&json_path, false, -6.0).unwrap();
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(doc["audio"]["levels"]["output_gain_db"], -6.0);
    assert_eq!(doc["audio"]["output_device"], "USB");
    assert_eq!(doc["other"]["keep"], true);
    assert_eq!(
        Config::load_from_file(&json_path)
            .unwrap()
            .audio
            .levels
            .output_gain_db,
        -6.0
    );
}

#[test]
fn save_drive_rejects_invalid_parent_without_replacing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, r#"{"audio":"wrong"}"#).unwrap();
    assert!(Config::save_output_gain_db(&path, false, -1.0).is_err());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        r#"{"audio":"wrong"}"#
    );
}

#[test]
fn higher_precedence_zero_resets_drive_and_files_are_ordered() {
    use crate::loader::{ConfigLoader, ConfigSource, SourceType};
    let dir = tempfile::tempdir().unwrap();
    let low = dir.path().join("low.toml");
    let high = dir.path().join("high.json");
    std::fs::write(&low, "[audio.levels]\noutput_gain_db = -6.0\n").unwrap();
    std::fs::write(&high, r#"{"audio":{"levels":{"output_gain_db":0.0}}}"#).unwrap();
    let mut loader = ConfigLoader::new().unwrap();
    for (path, priority, source_type) in [
        (low.clone(), 1, SourceType::Toml),
        (high.clone(), 2, SourceType::Json),
    ] {
        loader.add_source(ConfigSource {
            name: path.display().to_string(),
            path,
            priority,
            required: true,
            source_type,
        });
    }
    let config = loader.load().unwrap();
    assert_eq!(config.audio.levels.output_gain_db, 0.0);
    assert_eq!(loader.loaded_files(), vec![low, high]);
}
