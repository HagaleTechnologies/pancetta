// PAN-359. `[station] my_sig` / `my_sig_info`: loading and validation.
use super::*;

/// PAN-359 AC5: both keys load from TOML as `Option<String>` and default
/// to `None` when absent.
#[test]
fn my_sig_keys_load_from_toml() {
    let toml = r#"
        callsign = "K5ARH"
        grid_square = "EM12"
        my_sig = "POTA"
        my_sig_info = "K-1234"
    "#;
    let cfg: StationConfig = toml::from_str(toml).expect("parse");
    assert_eq!(cfg.my_sig.as_deref(), Some("POTA"));
    assert_eq!(cfg.my_sig_info.as_deref(), Some("K-1234"));
    assert!(cfg.validate_section().is_ok());

    let absent: StationConfig = toml::from_str("callsign = \"K5ARH\"").expect("parse");
    assert_eq!(absent.my_sig, None);
    assert_eq!(absent.my_sig_info, None);

    let default = StationConfig::default();
    assert_eq!(default.my_sig, None);
    assert_eq!(default.my_sig_info, None);
}

/// PAN-359 AC6: validation limits, counted after trimming, and the exact
/// diagnostic text. Each row sets one key against the valid default.
#[test]
fn my_sig_validation_table() {
    let long_sig = "P".repeat(MY_SIG_MAX_CHARS);
    let too_long_sig = "P".repeat(MY_SIG_MAX_CHARS + 1);
    let long_info = "K".repeat(MY_SIG_INFO_MAX_CHARS);
    let too_long_info = "K".repeat(MY_SIG_INFO_MAX_CHARS + 1);
    let padded_info = format!("  {long_info}  ");

    // (label, my_sig, my_sig_info, ok)
    let rows: Vec<(&str, Option<&str>, Option<&str>, bool)> = vec![
        ("both unset", None, None, true),
        ("both empty", Some(""), Some(""), true),
        ("both blank", Some("   "), Some("\t "), true),
        ("pota", Some("POTA"), Some("K-1234"), true),
        ("sota", Some("SOTA"), Some("W2/WE-003"), true),
        ("lower case kept valid", Some("pota"), Some("k-1234"), true),
        ("my_sig at limit", Some(&long_sig), None, true),
        ("my_sig over limit", Some(&too_long_sig), None, false),
        ("my_sig_info at limit", None, Some(&long_info), true),
        ("my_sig_info over limit", None, Some(&too_long_info), false),
        ("limit counted after trim", None, Some(&padded_info), true),
        ("inner space allowed", Some("WWFF X"), None, true),
        ("less-than", None, Some("K-12<34"), false),
        ("greater-than", Some("PO>TA"), None, false),
        ("control char", None, Some("K-12\u{7}"), false),
        ("inner tab", None, Some("K-1\t234"), false),
        ("non-ascii", Some("PÖTA"), None, false),
    ];
    for (label, sig, info, ok) in rows {
        let mut cfg = StationConfig::default();
        cfg.my_sig = sig.map(str::to_string);
        cfg.my_sig_info = info.map(str::to_string);
        assert_eq!(cfg.validate_section().is_ok(), ok, "row {label:?}");
    }

    let mut cfg = StationConfig::default();
    cfg.my_sig_info = Some("K-12<34".to_string());
    let err = cfg.validate_section().expect_err("must fail").to_string();
    assert_eq!(
        err,
        "Validation error: station.my_sig_info must be at most 64 printable ASCII characters other than < and >, got \"K-12<34\""
    );

    let mut cfg = StationConfig::default();
    cfg.my_sig = Some(too_long_sig.clone());
    let err = cfg.validate_section().expect_err("must fail").to_string();
    assert_eq!(
        err,
        format!(
            "Validation error: station.my_sig must be at most 32 printable ASCII characters other than < and >, got {too_long_sig:?}"
        )
    );

    // A control character is escaped, never written raw to the terminal.
    let mut cfg = StationConfig::default();
    cfg.my_sig_info = Some("K-12\u{7}".to_string());
    let err = cfg.validate_section().expect_err("must fail").to_string();
    assert!(err.ends_with("got \"K-12\\u{7}\""), "{err}");
}

/// PAN-359 AC5: `None` keeps the base value, `Some("")` clears it.
#[test]
fn my_sig_keys_merge_skip_none() {
    let mut base = StationConfig::default();
    base.my_sig = Some("POTA".to_string());
    base.my_sig_info = Some("K-1234".to_string());

    base.merge_with(StationConfig::default());
    assert_eq!(base.my_sig.as_deref(), Some("POTA"));
    assert_eq!(base.my_sig_info.as_deref(), Some("K-1234"));

    let mut overlay = StationConfig::default();
    overlay.my_sig = Some(String::new());
    overlay.my_sig_info = Some("K-0001".to_string());
    base.merge_with(overlay);
    assert_eq!(base.my_sig.as_deref(), Some(""));
    assert_eq!(base.my_sig_info.as_deref(), Some("K-0001"));
}
