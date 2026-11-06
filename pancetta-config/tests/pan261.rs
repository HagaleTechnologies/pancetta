use pancetta_config::{Config, DxTier};

#[test]
fn pan261_config_tiers_defaults_and_validation() {
    for (name, tier) in [
        ("atno", DxTier::Atno),
        ("per_band_dxcc_new", DxTier::PerBandDxccNew),
        ("special_station", DxTier::SpecialStation),
        ("per_band_grid_new", DxTier::PerBandGridNew),
        ("standard", DxTier::Standard),
        ("suspect", DxTier::Suspect),
    ] {
        let cfg: Config =
            toml::from_str(&format!("[autonomous]\npounce_min_tier = '{name}'")).unwrap();
        assert_eq!(cfg.autonomous.pounce_min_tier, Some(tier));
        assert_eq!(tier.to_string(), name);
    }
    assert!(toml::from_str::<Config>("[autonomous]\npounce_min_tier = 'unknown'").is_err());
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.autonomous.pounce_min_tier, None);
    assert_eq!(cfg.watchlist.ttl_secs, 150);
    assert!(cfg.watchlist.pinned.is_empty());
    assert_eq!(cfg.alerts.dedupe_secs, 300);
    assert!(!cfg.alerts.bell.enabled && !cfg.alerts.ntfy.enabled && !cfg.alerts.webhook.enabled);
    assert_eq!(cfg.alerts.ntfy.server, "https://ntfy.sh");
    assert_eq!(cfg.alerts.webhook.min_tier, DxTier::Atno);
    for (text, field) in [
        ("[alerts.ntfy]\nenabled = true", "alerts.ntfy.topic"),
        (
            "[alerts.webhook]\nenabled = true\nurl = 'ftp://host'",
            "alerts.webhook.url",
        ),
        ("[alerts]\ndedupe_secs = 0", "alerts.dedupe_secs"),
        ("[watchlist]\nttl_secs = 0", "watchlist.ttl_secs"),
    ] {
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains(field));
    }
}

#[test]
fn pan261_watchlist_persistence_preserves_comments_and_replaces_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config::default();
    cfg.set_watchlist_pinned_in_file(&path, &["VP8PJ".into()])
        .unwrap();
    assert_eq!(
        Config::load_from_file(&path).unwrap().watchlist.pinned,
        ["VP8PJ"]
    );
    std::fs::write(&path, "# station note\n[station]\ncallsign = 'K1ABC' # keep\n[watchlist]\nttl_secs = 42\npinned = ['VP8PJ']\n").unwrap();
    cfg.set_watchlist_pinned_in_file(&path, &["T88AA".into()])
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# station note") && text.contains("# keep"));
    let loaded = Config::load_from_file(&path).unwrap();
    assert_eq!(loaded.station.callsign, "K1ABC");
    assert_eq!(loaded.watchlist.ttl_secs, 42);
    assert_eq!(loaded.watchlist.pinned, ["T88AA"]);
    cfg.set_watchlist_pinned_in_file(&path, &[]).unwrap();
    assert!(Config::load_from_file(&path)
        .unwrap()
        .watchlist
        .pinned
        .is_empty());
}

#[test]
fn pan261_every_config_field_parses_and_merges() {
    let source: Config = toml::from_str(
        r#"
[autonomous]
pounce_min_tier = "special_station"
[watchlist]
ttl_secs = 200
pinned = ["VP8PJ"]
[alerts]
dedupe_secs = 123
[alerts.bell]
enabled = true
min_tier = "standard"
[alerts.ntfy]
enabled = true
min_tier = "per_band_grid_new"
server = "https://example.com"
topic = "dx"
[alerts.webhook]
enabled = true
min_tier = "per_band_dxcc_new"
url = "http://localhost/hook"
"#,
    )
    .unwrap();
    source.validate().unwrap();
    let mut merged = Config::default();
    merged.merge_with(source);
    assert_eq!(
        merged.autonomous.pounce_min_tier,
        Some(DxTier::SpecialStation)
    );
    assert_eq!(merged.watchlist.ttl_secs, 200);
    assert_eq!(merged.watchlist.pinned, ["VP8PJ"]);
    assert_eq!(merged.alerts.dedupe_secs, 123);
    assert!(merged.alerts.bell.enabled);
    assert_eq!(merged.alerts.bell.min_tier, DxTier::Standard);
    assert!(merged.alerts.ntfy.enabled);
    assert_eq!(merged.alerts.ntfy.min_tier, DxTier::PerBandGridNew);
    assert_eq!(merged.alerts.ntfy.server, "https://example.com");
    assert_eq!(merged.alerts.ntfy.topic, "dx");
    assert!(merged.alerts.webhook.enabled);
    assert_eq!(merged.alerts.webhook.min_tier, DxTier::PerBandDxccNew);
    assert_eq!(merged.alerts.webhook.url, "http://localhost/hook");
}
