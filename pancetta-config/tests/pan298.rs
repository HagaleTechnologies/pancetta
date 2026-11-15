use pancetta_config::Config;

#[test]
fn attribution_comment_defaults_off() {
    assert!(!Config::default().logging.attribution_comment);
}

#[test]
fn logging_section_loads_without_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[logging]\nattribution_comment = true\n").unwrap();
    let (config, warnings) = Config::load_from_file_with_warnings(path).unwrap();
    assert!(config.logging.attribution_comment);
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn config_merge_carries_attribution_comment() {
    let mut config = Config::default();
    let other: Config = toml::from_str("[logging]\nattribution_comment = true").unwrap();
    config.merge_with(other);
    assert!(config.logging.attribution_comment);
    config.merge_with(Config::default());
    assert!(!config.logging.attribution_comment);
}
