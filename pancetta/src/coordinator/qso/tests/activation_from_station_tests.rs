//! PAN-359: `[station] my_sig` / `my_sig_info` reach `QsoManagerConfig`.

use super::activation_from_station;
use pancetta_config::StationConfig;
use pancetta_qso::ActivationReference;

fn station(my_sig: Option<&str>, my_sig_info: Option<&str>) -> StationConfig {
    StationConfig {
        my_sig: my_sig.map(str::to_string),
        my_sig_info: my_sig_info.map(str::to_string),
        ..StationConfig::default()
    }
}

#[test]
fn activation_from_station_trims_and_drops_blank() {
    assert_eq!(
        activation_from_station(&station(Some(" POTA "), Some("\tK-1234 "))),
        ActivationReference {
            my_sig: Some("POTA".to_string()),
            my_sig_info: Some("K-1234".to_string()),
        },
        "values are trimmed and keep their case"
    );
    assert_eq!(
        activation_from_station(&station(Some("sota"), Some("w2/WE-003"))),
        ActivationReference {
            my_sig: Some("sota".to_string()),
            my_sig_info: Some("w2/WE-003".to_string()),
        },
        "case is kept as configured"
    );
    assert_eq!(
        activation_from_station(&station(Some(""), Some("   "))),
        ActivationReference::default(),
        "empty and blank values mean unset"
    );
    assert_eq!(
        activation_from_station(&station(None, None)),
        ActivationReference::default(),
        "absent keys mean unset"
    );
    assert_eq!(
        activation_from_station(&station(Some("POTA"), None)),
        ActivationReference {
            my_sig: Some("POTA".to_string()),
            my_sig_info: None,
        },
        "each key stands on its own"
    );
    assert!(activation_from_station(&StationConfig::default()).is_empty());
}
