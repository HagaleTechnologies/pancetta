//! Guard: every table in the config schema accepts a partial table (PAN-90).
//!
//! Walks Config::default() as TOML, and for every table path P parses a
//! document containing only `[P]`. Each must parse, and (unless listed in
//! PARSE_ONLY) the result must equal Config::default(): an omitted key always
//! means "the documented default". Only two kinds of allowlist entry exist:
//! RECORD_TABLES (map entries / records keyed by user-chosen names, which
//! stay strict) and PARSE_ONLY (map-valued fields, where an explicit table
//! replaces the whole map, or a struct type reused at sibling fields with
//! different defaults). Every entry carries its reason.
//!
//! A new config struct that lacks container-level `#[serde(default)]` (and a
//! `Default` impl) fails `every_config_table_accepts_an_empty_table`.
use pancetta_config::{CatInterfaceConfig, Config, SlotParitySetting};

const RECORD_TABLES: &[(&str, &str)] = &[(
    "ui.keyboard.shortcuts.",
    "entries of HashMap<String, KeyboardShortcut>; each entry is a complete record",
)];
const PARSE_ONLY: &[(&str, &str)] = &[
    (
        "ui.keyboard.shortcuts",
        "HashMap field: an explicit table replaces the map",
    ),
    (
        "ui.colors.secondary",
        "ColorPalette is reused for primary/secondary with different defaults; \
         ui.colors has no reader outside pancetta-config",
    ),
];

fn defaults_table() -> toml::Table {
    let mut c = Config::default();
    c.metadata = None;
    toml::Table::try_from(&c).unwrap()
}

fn table_paths(t: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (k, v) in t {
        let p = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if let toml::Value::Table(sub) = v {
            out.push(p.clone());
            table_paths(sub, &p, out);
        }
    }
}

#[test]
fn every_config_table_accepts_an_empty_table() {
    let defaults = defaults_table();
    let mut paths = Vec::new();
    table_paths(&defaults, "", &mut paths);
    assert!(
        paths.len() >= 140,
        "walker found only {} table paths",
        paths.len()
    );
    let mut failures = Vec::new();
    for p in &paths {
        if RECORD_TABLES
            .iter()
            .any(|(prefix, _)| p.starts_with(prefix))
        {
            continue;
        }
        match toml::from_str::<Config>(&format!("[{p}]\n")) {
            Err(e) => failures.push(format!("[{p}] does not parse: {e}")),
            Ok(parsed) => {
                assert!(
                    parsed.metadata.is_none(),
                    "[{p}]: metadata must stay None when absent"
                );
                let parsed = toml::Table::try_from(&parsed).unwrap();
                if !PARSE_ONLY.iter().any(|(q, _)| q == p) && parsed != defaults {
                    failures.push(format!("[{p}] parses but differs from Config::default()"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The allowlists stay short and never cover an operator-facing section.
#[test]
fn allowlists_stay_small_and_off_protected_sections() {
    const PROTECTED: &[&str] = &[
        "station",
        "audio",
        "rig.interface",
        "rig.ptt",
        "autonomous",
        "network.wsjtx_udp",
        "network.psk_reporter",
        "network.cqdx",
        "decoder",
        "duplicate_checking",
        "database",
    ];
    assert!(RECORD_TABLES.len() + PARSE_ONLY.len() <= 4);
    for (path, reason) in RECORD_TABLES.iter().chain(PARSE_ONLY) {
        assert!(!reason.is_empty(), "{path}: allowlist entry needs a reason");
        let path = path.trim_end_matches('.');
        for p in PROTECTED {
            // `audio` protects only the top-level table itself.
            let hit = if *p == "audio" {
                path == "audio"
            } else {
                path == *p || path.starts_with(&format!("{p}."))
            };
            assert!(!hit, "allowlist entry {path} is under protected [{p}]");
        }
    }
}

fn load_recipe(text: &str) -> Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pancetta.toml");
    std::fs::write(&path, text).unwrap();
    Config::load_from_file(&path)
        .unwrap_or_else(|e| panic!("recipe failed to load: {e}\n---\n{text}"))
}

/// README.md: hands-off operation is opt-in (`[autonomous] enabled = true`).
#[test]
fn readme_autonomous_toggle_parses() {
    let c = load_recipe("[autonomous]\nenabled = true\n");
    assert!(c.autonomous.enabled);
    assert_eq!(c.autonomous.slot_parity, SlotParitySetting::Auto);
}

/// docs/GUIDE.md "Same host" GridTracker recipe.
#[test]
fn guide_same_host_udp_recipe_parses() {
    let c = load_recipe(
        r#"[network.wsjtx_udp]
enabled = true
destination = "127.0.0.1:2237"
"#,
    );
    let udp = &c.network.wsjtx_udp;
    assert!(udp.enabled);
    assert_eq!(udp.destination, "127.0.0.1:2237");
    assert_eq!(udp.multicast_interface, "");
    assert_eq!(udp.instance_id, "WSJT-X - pancetta");
}

/// docs/GUIDE.md "Cross-machine" GridTracker recipe, comments included.
#[test]
fn guide_cross_machine_udp_recipe_parses() {
    let c = load_recipe(
        r#"[network.wsjtx_udp]
enabled = true
destination = "224.0.0.73:2237"     # multicast group; GridTracker's recommended choice
multicast_interface = "192.168.1.50" # A's real LAN NIC IP — NOT "" (loopback-only default)
multicast_ttl = 3
"#,
    );
    let udp = &c.network.wsjtx_udp;
    assert!(udp.enabled);
    assert_eq!(udp.multicast_interface, "192.168.1.50");
    assert_eq!(udp.multicast_ttl, 3);
    assert_eq!(udp.instance_id, "WSJT-X - pancetta");
}

/// docs/CONFIG.md "Minimum viable config".
#[test]
fn config_md_minimum_viable_config_parses() {
    let c = load_recipe(
        r#"[station]
callsign = "YOURCALL"        # Your FCC/ITU-issued callsign
grid_square = "FN42"         # 4-character Maidenhead grid

[audio]
input_device = "USB Audio CODEC"
output_device = "USB Audio CODEC"

[rig.interface]
enabled = true
port = "/dev/tty.usbserial-A1"
baud_rate = 38400

[rig]
model = "FTdx10"
"#,
    );
    assert_eq!(c.rig.interface.port, "/dev/tty.usbserial-A1");
    assert_eq!(
        c.rig.interface.data_bits,
        CatInterfaceConfig::default().data_bits
    );
    assert_eq!(c.rig.model, "FTdx10");
}

/// An old example file's partial `[metadata]` table must not fail the load.
#[test]
fn partial_metadata_table_parses() {
    let c = load_recipe("[metadata]\nversion = \"1.0\"\n");
    let meta = c.metadata.expect("metadata present");
    assert_eq!(meta.version, "1.0");
    assert!(meta.sources.is_empty());
}
