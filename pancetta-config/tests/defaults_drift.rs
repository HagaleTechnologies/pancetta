//! Drift guard: `pancetta-config/defaults.toml` is GENERATED from
//! `Config::default()` and must always match it.
//!
//! The file is documentation — the runtime never reads it: the loader's
//! "defaults" source returns `Config::default()` directly
//! (src/loader.rs, `load_embedded_defaults`). This test keeps the
//! documentation byte-honest. Same drift-fails-a-test philosophy as the
//! `merge_with` guard in src/lib.rs. The renderer lives in the library
//! (`Config::defaults_toml`) because `pancetta config --generate` writes the
//! same text (PAN-90).

use pancetta_config::Config;

#[test]
fn defaults_toml_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/defaults.toml");
    let expected = Config::defaults_toml();
    if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
        std::fs::write(path, &expected).expect("write defaults.toml");
        return;
    }
    let actual = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "pancetta-config/defaults.toml is stale. Regenerate with:\n  \
         PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test defaults_drift"
    );
}

/// Serialize a `Config` to TOML with deterministic key order (see
/// `Config::defaults_toml` for why this routes through `toml::Value`).
fn stable_toml(cfg: &Config) -> String {
    let value = toml::Value::try_from(cfg).expect("Config must convert to a toml::Value");
    toml::to_string_pretty(&value).expect("Config must serialize to TOML")
}

#[test]
fn generated_defaults_round_trip() {
    // The generated file must parse back into a Config equal (via re-serialize)
    // to what produced it — guards against serialize-only fields.
    let text = Config::defaults_toml();
    let reparsed: Config = toml::from_str(&text).expect("generated defaults.toml must parse");
    let original = Config {
        metadata: None,
        ..Default::default()
    };
    let mut reparsed = reparsed;
    reparsed.metadata = None;
    assert_eq!(stable_toml(&reparsed), stable_toml(&original));
}
