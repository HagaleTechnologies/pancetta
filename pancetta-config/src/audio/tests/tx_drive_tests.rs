use crate::{Config, ConfigSection};

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
