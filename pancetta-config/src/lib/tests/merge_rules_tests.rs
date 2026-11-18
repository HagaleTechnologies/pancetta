//! PAN-371: populated-base merge rules.
//!
//! `merge_guard::merge_with_carries_every_field` merges a fully non-default
//! source into `Default`, so it proves a field is carried but cannot see what
//! happens when a *populated* lower layer meets a higher layer. This test pins
//! that per field: every field not listed in a type's rule table must be
//! replaced by the overlay, and every listed field must follow its rule.

use super::merge_guard::{build_source_probed, collect_leaves, get_path, set_path, Probe, Seg};
use super::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// How one field merges when the base is populated. Unlisted fields are
/// replaced by the overlay's value, whatever it is.
#[derive(Clone, Copy, Debug)]
enum Rule {
    /// The overlay wins unless its value is unset: `""`, `0`, `[]` or absent.
    SkipEmpty,
    /// The overlay wins unless it is `None` (`Some(empty)` still wins).
    SkipNone,
    /// Map union; the overlay wins a key both layers set.
    Extend,
    /// Boolean OR: once any layer sets it, it stays set.
    Or,
}
use Rule::*;

type Rules = &'static [(&'static str, Rule)];

const STATION_RULES: Rules = &[
    ("/callsign", SkipEmpty),
    ("/grid_square", SkipEmpty),
    ("/power_watts", SkipEmpty),
    ("/qth", SkipEmpty),
    ("/dxcc_entity", SkipEmpty),
    ("/itu_zone", SkipEmpty),
    ("/cq_zone", SkipEmpty),
    ("/coordinates", SkipNone),
    ("/antennas", SkipEmpty),
    ("/operator_name", SkipNone),
    ("/contest_category", SkipNone),
    ("/tx_late_max_ms", SkipEmpty),
    ("/ptt_lead_ms", SkipEmpty),
    ("/custom_fields", Extend),
];

const AUDIO_RULES: Rules = &[
    ("/input_device", SkipEmpty),
    ("/output_device", SkipEmpty),
    ("/sample_rate", SkipEmpty),
    ("/buffer_size", SkipEmpty),
    ("/input_channels", SkipEmpty),
    ("/output_channels", SkipEmpty),
    ("/device_parameters", Extend),
];

const CAT_RULES: Rules = &[
    ("/port", SkipEmpty),
    ("/baud_rate", SkipEmpty),
    ("/data_bits", SkipEmpty),
    ("/timeout_ms", SkipEmpty),
    ("/termination", SkipEmpty),
    ("/response_timeout_ms", SkipEmpty),
    ("/retry_count", SkipEmpty),
];

const RIG_RULES: Rules = &[
    ("/model", SkipEmpty),
    ("/mode", SkipEmpty),
    ("/data_mode", SkipNone),
    ("/data_passband_hz", SkipNone),
    ("/bookmarks", SkipNone),
    ("/receive_gain_presets", SkipNone),
    ("/interface/port", SkipEmpty),
    ("/interface/baud_rate", SkipEmpty),
    ("/interface/data_bits", SkipEmpty),
    ("/interface/timeout_ms", SkipEmpty),
    ("/interface/termination", SkipEmpty),
    ("/interface/response_timeout_ms", SkipEmpty),
    ("/interface/retry_count", SkipEmpty),
    ("/custom_commands", Extend),
];

const UI_RULES: Rules = &[
    ("/theme", SkipEmpty),
    ("/layout", SkipEmpty),
    ("/accessibility/high_contrast", Or),
    ("/custom_widgets", Extend),
];

const NETWORK_RULES: Rules = &[("/custom_services", Extend)];

const REPLACE: Rules = &[];

/// Every value the higher layer can set back to "unset": strings `""`,
/// numbers `0`, arrays `[]`, `Option`s `None`, booleans at their default.
/// Each change is kept only if the result still deserializes.
fn emptied<T: Default + Serialize + DeserializeOwned>() -> T {
    let mut working = serde_json::to_value(T::default()).unwrap();
    let mut leaves = Vec::new();
    collect_leaves(&working, &mut Vec::new(), &mut leaves);
    for path in &leaves {
        let zero = match get_path(&working, path) {
            Some(Value::String(s)) if !s.is_empty() => json!(""),
            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => json!(0),
            _ => continue,
        };
        try_set::<T>(&mut working, path, zero);
    }
    let mut nodes = Vec::new();
    collect_nodes(&working, &mut Vec::new(), &mut nodes);
    for path in &nodes {
        let unset = match get_path(&working, path) {
            Some(Value::Array(a)) if !a.is_empty() => json!([]),
            Some(Value::Null) | None => continue,
            Some(_) => Value::Null,
        };
        try_set::<T>(&mut working, path, unset);
    }
    serde_json::from_value(working).expect("emptied value must deserialize")
}

fn collect_nodes(v: &Value, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    if !path.is_empty() {
        out.push(path.clone());
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                path.push(Seg::Key(k.clone()));
                collect_nodes(val, path, out);
                path.pop();
            }
        }
        Value::Array(a) => {
            for (i, val) in a.iter().enumerate() {
                path.push(Seg::Idx(i));
                collect_nodes(val, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn try_set<T: DeserializeOwned>(working: &mut Value, path: &[Seg], v: Value) {
    if get_path(working, path).is_none() {
        return;
    }
    let mut trial = working.clone();
    set_path(&mut trial, path, v);
    if serde_json::from_value::<T>(trial.clone()).is_ok() {
        *working = trial;
    }
}

fn is_unset(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

fn put(root: &mut Value, ptr: &str, v: Option<Value>) {
    let (parent, key) = ptr.rsplit_once('/').expect("rule path starts with /");
    let target = if parent.is_empty() {
        root
    } else {
        root.pointer_mut(parent).expect("rule parent exists")
    };
    let obj = target.as_object_mut().expect("rule parent is an object");
    match v {
        Some(v) => {
            obj.insert(key.to_string(), v);
        }
        None => {
            obj.remove(key);
        }
    }
}

/// The merge the rule table promises: the overlay, with each listed path
/// patched by its rule.
fn expected(base: &Value, overlay: &Value, rules: &[(String, Rule)]) -> Value {
    let mut out = overlay.clone();
    for (ptr, rule) in rules {
        let b = base.pointer(ptr);
        let o = overlay.pointer(ptr);
        let v = match rule {
            SkipEmpty => if is_unset(o) { b } else { o }.cloned(),
            SkipNone => if o.is_none_or(Value::is_null) { b } else { o }.cloned(),
            Extend => {
                let mut m: Map<String, Value> =
                    b.and_then(Value::as_object).cloned().unwrap_or_default();
                if let Some(om) = o.and_then(Value::as_object) {
                    for (k, v) in om {
                        m.insert(k.clone(), v.clone());
                    }
                }
                Some(Value::Object(m))
            }
            Or => Some(Value::Bool(
                b.and_then(Value::as_bool).unwrap_or(false)
                    || o.and_then(Value::as_bool).unwrap_or(false),
            )),
        };
        put(&mut out, ptr, v);
    }
    out
}

fn diff_paths(got: &Value, want: &Value, at: String, out: &mut Vec<String>) {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            let keys: BTreeSet<&String> = g.keys().chain(w.keys()).collect();
            for k in keys {
                match (g.get(k), w.get(k)) {
                    (Some(gv), Some(wv)) => diff_paths(gv, wv, format!("{at}/{k}"), out),
                    (gv, wv) => out.push(format!("{at}/{k}: got {gv:?}, want {wv:?}")),
                }
            }
        }
        _ if got != want => out.push(format!("{at}: got {got}, want {want}")),
        _ => {}
    }
}

struct Fixtures {
    base: Value,
    overlay: Value,
    empty: Value,
    cleared: Value,
}

fn owned(rules: Rules, prefix: &str) -> Vec<(String, Rule)> {
    rules
        .iter()
        .map(|(p, r)| (format!("{prefix}{p}"), *r))
        .collect()
}

fn assert_merge_result(
    name: &str,
    label: &str,
    base: &Value,
    overlay: &Value,
    got: &Value,
    rules: &[(String, Rule)],
) {
    let want = expected(base, overlay, rules);
    let mut diffs = Vec::new();
    diff_paths(got, &want, String::new(), &mut diffs);
    assert!(
        diffs.is_empty(),
        "{name}: merging the {label} overlay into a populated base broke its rule table \
         (an unlisted field must take the overlay's value; a listed one must follow its rule):\n  {}",
        diffs.join("\n  ")
    );
}

/// Merge a populated, an emptied and a cleared overlay into a populated base
/// and compare each result with the rule table's promise.
fn assert_rules<T>(
    name: &str,
    base_overrides: &[(&str, Value)],
    overlay_overrides: &[(&str, Value)],
    rules: Rules,
    merge: impl Fn(&mut T, T),
) -> Fixtures
where
    T: Default + Clone + Serialize + DeserializeOwned,
{
    let base: T = build_source_probed(base_overrides, Probe::Base);
    let overlay: T = build_source_probed(overlay_overrides, Probe::Overlay);
    let empty: T = emptied();
    let bj = serde_json::to_value(&base).unwrap();
    let oj = serde_json::to_value(&overlay).unwrap();
    let zj = serde_json::to_value(&empty).unwrap();
    let rules_owned = owned(rules, "");

    for (ptr, rule) in rules {
        assert_ne!(
            bj.pointer(ptr),
            zj.pointer(ptr),
            "{name}: test setup bug: base and emptied overlay agree at {ptr}, so {rule:?} cannot be observed"
        );
        if let Extend = rule {
            let b = bj
                .pointer(ptr)
                .and_then(Value::as_object)
                .expect("extend base is a map");
            let o = oj
                .pointer(ptr)
                .and_then(Value::as_object)
                .expect("extend overlay is a map");
            assert!(
                !b.is_empty() && !o.is_empty() && b.keys().all(|k| !o.contains_key(k)),
                "{name}: test setup bug: {ptr} needs disjoint non-empty base and overlay maps"
            );
        }
    }

    // `Some(empty)` is a real value for a SkipNone field: it must clear the
    // base, not fall through to it.
    let mut cleared_json = zj.clone();
    for (ptr, rule) in rules {
        let empty_value = match (rule, bj.pointer(ptr)) {
            (SkipNone, Some(Value::Array(_))) => json!([]),
            (SkipNone, Some(Value::Object(_))) => json!({}),
            _ => continue,
        };
        let mut trial = cleared_json.clone();
        put(&mut trial, ptr, Some(empty_value));
        if serde_json::from_value::<T>(trial.clone()).is_ok() {
            cleared_json = trial;
        }
    }
    let cleared: T = serde_json::from_value(cleared_json).unwrap();
    let cj = serde_json::to_value(&cleared).unwrap();

    for (label, ov, ovj) in [
        ("populated", overlay, &oj),
        ("emptied", empty, &zj),
        ("cleared", cleared, &cj),
    ] {
        let mut merged = base.clone();
        merge(&mut merged, ov);
        let got = serde_json::to_value(&merged).unwrap();
        assert_merge_result(name, label, &bj, ovj, &got, &rules_owned);
    }
    Fixtures {
        base: bj,
        overlay: oj,
        empty: zj,
        cleared: cj,
    }
}

#[test]
fn populated_base_merge_follows_each_fields_rule() {
    let mut sections: Vec<(&str, Fixtures, Rules)> = Vec::new();

    sections.push((
        "station",
        assert_rules::<station::StationConfig>(
            "StationConfig",
            &[
                (
                    "coordinates",
                    json!({"latitude": 12.5, "longitude": -34.5, "elevation_meters": 56.5}),
                ),
                ("operator_name", json!("Base Op")),
                ("contest_category", json!("SINGLE-OP")),
                ("tx_self_parity", json!("even")),
                ("custom_fields", json!({"base_field": "b"})),
            ],
            &[
                (
                    "coordinates",
                    json!({"latitude": -7.5, "longitude": 101.5, "elevation_meters": 3.5}),
                ),
                ("operator_name", json!("Overlay Op")),
                ("contest_category", json!("MULTI-ONE")),
                ("tx_self_parity", json!("odd")),
                ("custom_fields", json!({"overlay_field": "o"})),
            ],
            STATION_RULES,
            |a, b| a.merge_with(b),
        ),
        STATION_RULES,
    ));

    sections.push((
        "audio",
        assert_rules::<audio::AudioConfig>(
            "AudioConfig",
            &[
                ("bit_depth", json!("int16")),
                ("device_parameters", json!({"base_param": "1"})),
            ],
            &[
                ("bit_depth", json!("int24")),
                ("device_parameters", json!({"overlay_param": "2"})),
            ],
            AUDIO_RULES,
            |a, b| a.merge_with(b),
        ),
        AUDIO_RULES,
    ));

    let widget = |t: &str| json!({"widget_type": t, "properties": {}, "visible": true});
    sections.push((
        "ui",
        assert_rules::<ui::UiConfig>(
            "UiConfig",
            &[
                ("waterfall", json!({"palette": "viridis", "gain_db": 7.5})),
                ("custom_widgets", json!({"base_widget": widget("clock")})),
            ],
            &[
                ("waterfall", json!({"palette": "thermal", "gain_db": -3.5})),
                ("custom_widgets", json!({"overlay_widget": widget("meter")})),
            ],
            UI_RULES,
            |a, b| a.merge_with(b),
        ),
        UI_RULES,
    ));

    let bookmark = |n: &str| json!([{"name": n, "model": "FTdx10", "port": "/dev/ttyUSB0", "baud_rate": 38400, "ptt_method": "cat"}]);
    sections.push((
        "rig",
        assert_rules::<rig::RigConfig>(
            "RigConfig",
            &[
                ("data_mode", json!("PKTUSB")),
                ("data_passband_hz", json!(3000)),
                ("bookmarks", bookmark("Base")),
                (
                    "receive_gain_presets",
                    json!({"10m": {"preamp_db": 10, "attenuator_db": 6}}),
                ),
                ("custom_commands", json!({"base_cmd": "FA;"})),
            ],
            &[
                ("data_mode", json!("DATA-U")),
                ("data_passband_hz", json!(2400)),
                ("bookmarks", bookmark("Overlay")),
                (
                    "receive_gain_presets",
                    json!({"20m": {"preamp_db": 0, "attenuator_db": 12}}),
                ),
                ("custom_commands", json!({"overlay_cmd": "FB;"})),
            ],
            RIG_RULES,
            |a, b| a.merge_with(b),
        ),
        RIG_RULES,
    ));

    assert_rules::<rig::CatInterfaceConfig>(
        "CatInterfaceConfig",
        &[
            ("stop_bits", json!("two")),
            ("parity", json!("even")),
            ("flow_control", json!("hardware")),
            ("protocol", json!("yaesu")),
        ],
        &[
            ("stop_bits", json!("one")),
            ("parity", json!("odd")),
            ("flow_control", json!("software")),
            ("protocol", json!("kenwood")),
        ],
        CAT_RULES,
        |a, b| a.merge_with(b),
    );

    let service = |n: &str| json!({"name": n, "url": "https://localhost", "service_type": "rest", "auth": null, "headers": {}, "parameters": {}, "enabled": true});
    sections.push((
        "network",
        assert_rules::<network::NetworkConfig>(
            "NetworkConfig",
            &[("custom_services", json!({"base_service": service("b")}))],
            &[("custom_services", json!({"overlay_service": service("o")}))],
            NETWORK_RULES,
            |a, b| a.merge_with(b),
        ),
        NETWORK_RULES,
    ));

    assert_rules::<network::RemoteGatewayConfig>(
        "RemoteGatewayConfig",
        &[
            ("auth_token", json!("t".repeat(40))),
            ("allowed_origins", json!(["http://localhost:8081"])),
        ],
        &[
            ("auth_token", json!("u".repeat(40))),
            ("allowed_origins", json!(["http://localhost:9091"])),
        ],
        REPLACE,
        |a, b| a.merge_with(b),
    );
    assert_rules::<network::WsjtxUdpConfig>(
        "WsjtxUdpConfig",
        &[("allowed_request_hosts", json!(["example.com"]))],
        &[("allowed_request_hosts", json!(["example.org"]))],
        REPLACE,
        |a, b| a.merge_with(b),
    );

    sections.push((
        "autonomous",
        assert_rules::<autonomous::AutonomousConfig>(
            "AutonomousConfig",
            &[
                ("needed_means", json!("unconfirmed")),
                ("slot_parity", json!("even")),
                ("unattended_policy", json!("respond_only")),
                ("pounce_min_tier", json!("atno")),
                (
                    "band_plan",
                    json!([{"utc": "00:00-06:00", "bands": ["40m", "80m"]}]),
                ),
            ],
            &[
                ("needed_means", json!("unconfirmed")),
                ("slot_parity", json!("odd")),
                ("unattended_policy", json!("respond_only")),
                ("pounce_min_tier", json!("standard")),
                (
                    "band_plan",
                    json!([{"utc": "06:00-12:00", "bands": ["20m"]}]),
                ),
            ],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));

    sections.push((
        "alerts",
        assert_rules::<AlertsConfig>(
            "AlertsConfig",
            &[
                ("bell", json!({"enabled": true, "min_tier": "standard"})),
                ("ntfy", json!({"enabled": true, "min_tier": "special_station", "server": "https://localhost", "topic": "dx"})),
                ("webhook", json!({"enabled": true, "min_tier": "per_band_dxcc_new", "url": "https://localhost/hook"})),
            ],
            &[
                ("bell", json!({"enabled": false, "min_tier": "atno"})),
                ("ntfy", json!({"enabled": false, "min_tier": "standard", "server": "https://example.org", "topic": "cq"})),
                ("webhook", json!({"enabled": false, "min_tier": "atno", "url": "https://example.org/hook"})),
            ],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));
    sections.push((
        "watchlist",
        assert_rules::<WatchlistConfig>(
            "WatchlistConfig",
            &[("pinned", json!(["VP8PJ"]))],
            &[("pinned", json!(["3Y0J"]))],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));
    sections.push((
        "hound",
        assert_rules::<hound::HoundConfig>("HoundConfig", &[], &[], REPLACE, |a, b| {
            a.merge_with(b)
        }),
        REPLACE,
    ));
    sections.push((
        "fox",
        assert_rules::<fox::FoxConfig>("FoxConfig", &[], &[], REPLACE, |a, b| a.merge_with(b)),
        REPLACE,
    ));
    sections.push((
        "tx_placement",
        assert_rules::<tx_placement::TxPlacementConfig>(
            "TxPlacementConfig",
            &[],
            &[],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));
    sections.push((
        "decoder",
        assert_rules::<decoder::DecoderConfig>(
            "DecoderConfig",
            &[("effort", json!("eco")), ("budget_ms", json!(4321))],
            &[("effort", json!("deep")), ("budget_ms", json!(1234))],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));
    sections.push((
        "duplicate_checking",
        assert_rules::<duplicate_check::DuplicateCheckingConfig>(
            "DuplicateCheckingConfig",
            &[],
            &[],
            REPLACE,
            |a, b| a.merge_with(b),
        ),
        REPLACE,
    ));
    sections.push((
        "database",
        assert_rules::<database::DatabaseConfig>("DatabaseConfig", &[], &[], REPLACE, |a, b| {
            a.merge_with(b)
        }),
        REPLACE,
    ));
    sections.push((
        "logging",
        assert_rules::<LoggingConfig>("LoggingConfig", &[], &[], REPLACE, |a, b| a.merge_with(b)),
        REPLACE,
    ));

    let contest = assert_rules::<contest::ContestScoringConfig>(
        "ContestScoringConfig",
        &[
            ("instance_id", json!("KSQP-2026-K5ARH")),
            ("profile_id", json!("us-state-qso-party")),
            ("rules_id", json!("ksqp-ft-2026")),
            ("operator_region", json!("TX")),
            (
                "periods",
                json!([{ "start": "2026-08-29T14:00:00Z", "end": "2026-08-30T02:00:00Z" }]),
            ),
        ],
        &[
            ("instance_id", json!("NAQP-2026-K5ARH")),
            ("profile_id", json!("naqp")),
            ("rules_id", json!("naqp-ft-2026")),
            ("operator_region", json!("OK")),
            (
                "periods",
                json!([{ "start": "2026-09-05T18:00:00Z", "end": "2026-09-06T06:00:00Z" }]),
            ),
        ],
        REPLACE,
        |a, b| a.merge_with(b),
    );

    // Config::merge_with: every section delegates under its own key, and
    // `[contest]` replaces as a unit only when the overlay carries it.
    let default_keys: BTreeSet<String> = serde_json::to_value(Config::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.as_str() != "metadata")
        .cloned()
        .collect();
    let listed: BTreeSet<String> = sections.iter().map(|(k, _, _)| k.to_string()).collect();
    assert_eq!(
        listed, default_keys,
        "every Config section needs a populated-base case"
    );

    let mut rules: Vec<(String, Rule)> = vec![("/contest".to_string(), SkipNone)];
    let (mut b, mut o, mut z, mut c) = (Map::new(), Map::new(), Map::new(), Map::new());
    for (key, fx, section_rules) in &sections {
        rules.extend(owned(section_rules, &format!("/{key}")));
        b.insert(key.to_string(), fx.base.clone());
        o.insert(key.to_string(), fx.overlay.clone());
        z.insert(key.to_string(), fx.empty.clone());
        c.insert(key.to_string(), fx.cleared.clone());
    }
    b.insert("contest".into(), contest.base.clone());
    o.insert("contest".into(), contest.overlay.clone());
    let (bj, oj, zj, cj) = (
        Value::Object(b),
        Value::Object(o),
        Value::Object(z),
        Value::Object(c),
    );
    let base: Config = serde_json::from_value(bj.clone()).unwrap();
    for (label, ovj) in [("populated", &oj), ("emptied", &zj), ("cleared", &cj)] {
        let overlay: Config = serde_json::from_value(ovj.clone()).unwrap();
        let mut merged = base.clone();
        merged.merge_with(overlay);
        let got = serde_json::to_value(&merged).unwrap();
        assert_merge_result("Config", label, &bj, ovj, &got, &rules);
    }
}

/// `metadata` is bookkeeping, not operator config: the base keeps its own,
/// appends the overlay's sources, and is stamped with a fresh
/// `last_modified`.
#[test]
fn config_merge_keeps_base_metadata_and_appends_sources() {
    let stamp = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let meta = |version: &str, source: &str| {
        Some(ConfigMetadata {
            version: version.to_string(),
            last_modified: Some(stamp),
            sources: vec![std::path::PathBuf::from(source)],
            instance_id: uuid::Uuid::nil(),
        })
    };
    let mut base = Config {
        metadata: meta("base", "a.toml"),
        ..Config::default()
    };
    base.merge_with(Config {
        metadata: meta("overlay", "b.toml"),
        ..Config::default()
    });
    let m = base.metadata.as_ref().unwrap();
    assert_eq!(m.version, "base");
    assert_eq!(
        m.sources,
        vec![
            std::path::PathBuf::from("a.toml"),
            std::path::PathBuf::from("b.toml")
        ]
    );
    assert!(m.last_modified.unwrap() > stamp);

    base.merge_with(Config {
        metadata: None,
        ..Config::default()
    });
    assert_eq!(base.metadata.as_ref().unwrap().sources.len(), 2);

    let mut bare = Config {
        metadata: None,
        ..Config::default()
    };
    bare.merge_with(Config {
        metadata: meta("overlay", "b.toml"),
        ..Config::default()
    });
    assert!(bare.metadata.is_none());
}
