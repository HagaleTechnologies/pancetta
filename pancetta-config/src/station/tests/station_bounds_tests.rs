// PAN-315. Station bounds that validator 0.21 enforced through derive
// attributes, and the diagnostics grammar they produce.
use super::*;

/// PAN-315 AC5: the six bounds validator 0.21 enforced through derive
/// attributes keep their inclusive limits. Each row varies one field
/// against the valid default. Callsign length counts Unicode characters.
#[test]
fn station_derived_constraints_boundary_table() {
    type Edit = fn(&mut StationConfig);
    let rows: &[(&str, Edit, bool)] = &[
        ("callsign 3 chars", |c| c.callsign = "K1A".into(), true),
        ("callsign 2 chars", |c| c.callsign = "K1".into(), false),
        ("callsign empty", |c| c.callsign = String::new(), false),
        (
            "callsign 20 chars",
            |c| c.callsign = format!("K1{}", "A".repeat(18)),
            true,
        ),
        (
            "callsign 21 chars",
            |c| c.callsign = format!("K1{}", "A".repeat(19)),
            false,
        ),
        (
            "callsign 64 chars",
            |c| c.callsign = format!("K1{}", "A".repeat(62)),
            false,
        ),
        // 20 characters but 38 bytes: the limit counts characters.
        (
            "callsign 20 unicode chars",
            |c| c.callsign = format!("K1{}", "Ä".repeat(18)),
            true,
        ),
        (
            "callsign 21 unicode chars",
            |c| c.callsign = format!("K1{}", "Ä".repeat(19)),
            false,
        ),
        ("grid 4 chars", |c| c.grid_square = "FN42".into(), true),
        ("grid 8 chars", |c| c.grid_square = "FN42ab12".into(), true),
        ("grid 3 chars", |c| c.grid_square = "FN4".into(), false),
        (
            "grid 9 chars",
            |c| c.grid_square = "FN42ab12x".into(),
            false,
        ),
        ("power 1 W", |c| c.power_watts = 1, true),
        ("power 1500 W", |c| c.power_watts = 1500, true),
        ("power 0 W", |c| c.power_watts = 0, false),
        ("power 1501 W", |c| c.power_watts = 1501, false),
        ("dxcc 1", |c| c.dxcc_entity = 1, true),
        ("dxcc 999", |c| c.dxcc_entity = 999, true),
        ("dxcc 0", |c| c.dxcc_entity = 0, false),
        ("dxcc 1000", |c| c.dxcc_entity = 1000, false),
        ("itu 1", |c| c.itu_zone = 1, true),
        ("itu 90", |c| c.itu_zone = 90, true),
        ("itu 0", |c| c.itu_zone = 0, false),
        ("itu 91", |c| c.itu_zone = 91, false),
        ("cq 1", |c| c.cq_zone = 1, true),
        ("cq 40", |c| c.cq_zone = 40, true),
        ("cq 0", |c| c.cq_zone = 0, false),
        ("cq 41", |c| c.cq_zone = 41, false),
        // Existing semantic checks still run after the bounds pass.
        (
            "callsign without digit",
            |c| c.callsign = "KAAA".into(),
            false,
        ),
        ("grid outside A-R", |c| c.grid_square = "ZZ99".into(), false),
        (
            "latitude out of range",
            |c| {
                c.coordinates = Some(Coordinates {
                    latitude: 91.0,
                    longitude: 0.0,
                    elevation_meters: None,
                })
            },
            false,
        ),
        (
            "antenna azimuth 360",
            |c| c.antennas[0].azimuth_degrees = Some(360),
            false,
        ),
    ];
    for (label, edit, valid) in rows {
        let mut cfg = StationConfig::default();
        edit(&mut cfg);
        assert_eq!(cfg.validate_section().is_ok(), *valid, "{label}");
    }
}

/// Split a station diagnostic into per-field `(field, code, params)`
/// rows. validator stored fields and parameters in hash maps, so their
/// order was never stable; compare them as sets.
fn normalized_station_errors(message: &str) -> Vec<(String, String, Vec<String>)> {
    let body = message
        .strip_prefix("Validation error: Station validation failed: ")
        .unwrap_or_else(|| panic!("missing station prefix: {message}"));
    let mut rows: Vec<_> = body
        .lines()
        .map(|line| {
            let (field, rest) = line.split_once(": Validation error: ").unwrap();
            let (code, params) = rest.split_once(" [{").unwrap();
            let mut params: Vec<String> = params
                .strip_suffix("}]")
                .unwrap()
                .split(", ")
                .map(str::to_string)
                .collect();
            params.sort();
            (field.to_string(), code.to_string(), params)
        })
        .collect();
    rows.sort();
    rows
}

fn row(field: &str, code: &str, params: &[&str]) -> (String, String, Vec<String>) {
    let mut params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
    params.sort();
    (field.to_string(), code.to_string(), params)
}

/// PAN-315 AC5: diagnostics keep validator 0.21's grammar, the
/// `Station validation failed:` prefix and its debug-rendered
/// min/max/value parameters.
#[test]
fn station_derived_diagnostics_compatible() {
    let mut cfg = StationConfig::default();
    cfg.callsign = "K1".into();
    let message = cfg.validate_section().unwrap_err().to_string();
    assert_eq!(
        normalized_station_errors(&message),
        vec![row(
            "callsign",
            "length",
            &[
                r#""min": Number(3)"#,
                r#""max": Number(20)"#,
                r#""value": String("K1")"#
            ],
        )]
    );

    let mut cfg = StationConfig::default();
    cfg.grid_square = "FN4".into();
    cfg.power_watts = 1501;
    cfg.dxcc_entity = 0;
    cfg.itu_zone = 91;
    cfg.cq_zone = 0;
    let message = cfg.validate_section().unwrap_err().to_string();
    assert_eq!(
        normalized_station_errors(&message),
        vec![
            row(
                "cq_zone",
                "range",
                &[
                    r#""min": Number(1)"#,
                    r#""max": Number(40)"#,
                    r#""value": Number(0)"#
                ]
            ),
            row(
                "dxcc_entity",
                "range",
                &[
                    r#""min": Number(1)"#,
                    r#""max": Number(999)"#,
                    r#""value": Number(0)"#
                ]
            ),
            row(
                "grid_square",
                "length",
                &[
                    r#""min": Number(4)"#,
                    r#""max": Number(8)"#,
                    r#""value": String("FN4")"#
                ]
            ),
            row(
                "itu_zone",
                "range",
                &[
                    r#""min": Number(1)"#,
                    r#""max": Number(90)"#,
                    r#""value": Number(91)"#
                ]
            ),
            row(
                "power_watts",
                "range",
                &[
                    r#""min": Number(1)"#,
                    r#""max": Number(1500)"#,
                    r#""value": Number(1501)"#
                ]
            ),
        ]
    );
}
