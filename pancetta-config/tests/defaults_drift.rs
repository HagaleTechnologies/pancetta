//! Drift guard: `pancetta-config/defaults.toml` is GENERATED from
//! `Config::default()` and must always match it.
//!
//! The file is documentation — the runtime never reads it: the loader's
//! "defaults" source returns `Config::default()` directly
//! (src/loader.rs, `load_embedded_defaults`). This test keeps the
//! documentation byte-honest. Same drift-fails-a-test philosophy as the
//! `merge_with` guard in src/lib.rs.

use pancetta_config::Config;

const HEADER: &str = "\
# GENERATED FILE - do not edit by hand.
# This is the full pancetta configuration schema with every default value,
# serialized from Config::default() (the runtime source of truth; the loader
# never reads this file). Annotated key documentation: docs/CONFIG.md.
# Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test defaults_drift
";

fn render_defaults_toml() -> String {
    let mut cfg = Config::default();
    // metadata carries a fresh uuid + timestamp per construction — per-run
    // noise, not schema. Config's serde skips it when None.
    cfg.metadata = None;
    // Route through `toml::Value` (not `toml::to_string_pretty(&cfg)` directly)
    // before rendering. `Config::ui::keyboard::shortcuts` is a
    // `HashMap<String, KeyboardShortcut>` (pancetta-config/src/ui.rs); serde's
    // std-HashMap `Serialize` impl iterates in the map's own randomized-hasher
    // order, so serializing the struct straight to TOML text renders that one
    // table's key order differently on every process run (verified: ~80%
    // failure rate across 5 runs when serialized directly). `toml::map::Map`
    // (backing `Value::Table`, this workspace does not enable the `toml`
    // crate's `preserve_order` feature) is BTreeMap-backed, so bouncing
    // through `Value` first sorts every table's keys deterministically,
    // eliminating the flake without touching the `HashMap` field itself
    // (out of scope for this task — see docs/DECISIONS/config-and-platform.md).
    format!("{HEADER}\n{}", stable_toml(&cfg))
}

/// Serialize a `Config` to TOML with deterministic key order.
///
/// `toml::to_string_pretty(&cfg)` serializes struct fields directly, so a
/// `HashMap`-typed field (`Config::ui::keyboard::shortcuts`, see the note in
/// `render_defaults_toml`) renders its keys in that map's own
/// randomized-hasher order — different on every `Config::default()`
/// construction. Bouncing through `toml::Value` first sorts every table's
/// keys via `toml::map::Map`, which is `BTreeMap`-backed in this workspace
/// (the `toml` crate's `preserve_order` feature is not enabled), making the
/// output byte-stable across processes and across two `Config` instances
/// built by different paths (`Config::default()` vs. `toml::from_str`).
fn stable_toml(cfg: &Config) -> String {
    let value = toml::Value::try_from(cfg)
        .expect("Config must convert to a toml::Value (see plan Task 8 fallback)");
    toml::to_string_pretty(&value)
        .expect("Config must serialize to TOML (see plan Task 8 fallback)")
}

#[test]
fn defaults_toml_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/defaults.toml");
    let expected = render_defaults_toml();
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

#[test]
fn generated_defaults_round_trip() {
    // The generated file must parse back into a Config equal (via re-serialize)
    // to what produced it — guards against serialize-only fields.
    let text = render_defaults_toml();
    let reparsed: Config = toml::from_str(&text).expect("generated defaults.toml must parse");
    let mut original = Config::default();
    original.metadata = None;
    let mut reparsed = reparsed;
    reparsed.metadata = None;
    assert_eq!(stable_toml(&reparsed), stable_toml(&original));
}
